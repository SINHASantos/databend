// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use arrow_schema::Schema as ArrowSchema;
use common_base::runtime::execute_futures_in_parallel;
use common_base::runtime::GLOBAL_MEM_STAT;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::FieldIndex;
use opendal::BlockingOperator;
use opendal::Operator;
use parquet::arrow::parquet_to_arrow_schema;
use parquet::file::footer::decode_footer;
use parquet::file::footer::decode_metadata;
use parquet::file::metadata::ParquetMetaData;

pub const FOOTER_SIZE: u64 = 8;

#[async_backtrace::framed]
pub async fn read_parquet_schema_async_rs(operator: &Operator, path: &str) -> Result<ArrowSchema> {
    let meta = read_metadata_async(path, operator, None)
        .await
        .map_err(|e| {
            ErrorCode::Internal(format!("Read parquet file '{}''s meta error: {}", path, e))
        })?;

    infer_schema_with_extension(&meta)
}

pub fn infer_schema_with_extension(meta: &ParquetMetaData) -> Result<ArrowSchema> {
    let meta = meta.file_metadata();
    let arrow_schema = parquet_to_arrow_schema(meta.schema_descr(), meta.key_value_metadata())
        .map_err(ErrorCode::from_std_error)?;
    // todo: Convert data types to extension types using meta information.
    // Mainly used for types such as Variant and Bitmap,
    // as they have the same physical type as String.
    Ok(arrow_schema)
}

#[allow(dead_code)]
async fn read_parquet_metas_batch(
    file_infos: Vec<(String, u64)>,
    op: Operator,
    max_memory_usage: u64,
) -> Result<Vec<ParquetMetaData>> {
    let mut metas = vec![];
    for (path, size) in file_infos {
        metas.push(read_metadata_async(&path, &op, Some(size)).await?)
    }
    let used = GLOBAL_MEM_STAT.get_memory_usage();
    if max_memory_usage as i64 - used < 100 * 1024 * 1024 {
        Err(ErrorCode::Internal(format!(
            "not enough memory to load parquet file metas, max_memory_usage = {}, used = {}.",
            max_memory_usage, used
        )))
    } else {
        Ok(metas)
    }
}

#[async_backtrace::framed]
pub async fn read_parquet_metas_in_parallel(
    op: Operator,
    file_infos: Vec<(String, u64)>,
    thread_nums: usize,
    permit_nums: usize,
    max_memory_usage: u64,
) -> Result<Vec<ParquetMetaData>> {
    let batch_size = 100;
    if file_infos.len() <= batch_size {
        read_parquet_metas_batch(file_infos, op.clone(), max_memory_usage).await
    } else {
        let mut chunks = file_infos.chunks(batch_size);

        let tasks = std::iter::from_fn(move || {
            chunks.next().map(|location| {
                read_parquet_metas_batch(location.to_vec(), op.clone(), max_memory_usage)
            })
        });

        let result = execute_futures_in_parallel(
            tasks,
            thread_nums,
            permit_nums,
            "read-parquet-metas-worker".to_owned(),
        )
        .await?
        .into_iter()
        .collect::<Result<Vec<Vec<_>>>>()?
        .into_iter()
        .flatten()
        .collect();

        Ok(result)
    }
}

/// Layout of Parquet file
/// +---------------------------+-----+---+
/// |      Rest of file         |  B  | A |
/// +---------------------------+-----+---+
/// where A: parquet footer, B: parquet metadata.
///
/// The reader first reads DEFAULT_FOOTER_SIZE bytes from the end of the file.
/// If it is not enough according to the length indicated in the footer, it reads more bytes.
pub async fn read_metadata_async(
    path: &str,
    operator: &Operator,
    file_size: Option<u64>,
) -> Result<ParquetMetaData> {
    let file_size = match file_size {
        None => operator.stat(path).await?.content_length(),
        Some(n) => n,
    };
    check_footer_size(file_size)?;
    let footer = operator
        .range_read(path, (file_size - FOOTER_SIZE)..file_size)
        .await?;
    let metadata_len = decode_footer(&footer[0..8].try_into().unwrap())? as u64;
    check_meta_size(file_size, metadata_len)?;
    let metadata = operator
        .range_read(path, (file_size - FOOTER_SIZE - metadata_len)..file_size)
        .await?;
    Ok(decode_metadata(&metadata)?)
}

#[allow(dead_code)]
pub fn read_metadata(
    path: &str,
    operator: &BlockingOperator,
    file_size: u64,
) -> Result<ParquetMetaData> {
    check_footer_size(file_size)?;
    let footer = operator.range_read(path, (file_size - FOOTER_SIZE)..file_size)?;
    let metadata_len = decode_footer(&footer[0..8].try_into().unwrap())? as u64;
    check_meta_size(file_size, metadata_len)?;
    let metadata =
        operator.range_read(path, (file_size - FOOTER_SIZE - metadata_len)..file_size)?;
    Ok(decode_metadata(&metadata)?)
}

/// check file is large enough to hold footer
fn check_footer_size(file_size: u64) -> Result<()> {
    if file_size < FOOTER_SIZE {
        Err(ErrorCode::BadBytes(
            "Invalid Parquet file. Size is smaller than footer.",
        ))
    } else {
        Ok(())
    }
}

/// check file is large enough to hold metadata
fn check_meta_size(file_size: u64, metadata_len: u64) -> Result<()> {
    if metadata_len + FOOTER_SIZE > file_size {
        Err(ErrorCode::BadBytes(format!(
            "Invalid Parquet file. Reported metadata length of {} + {} byte footer, but file is only {} bytes",
            metadata_len, FOOTER_SIZE, file_size
        )))
    } else {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParquetSchemaTreeNode {
    Leaf(usize),
    Inner(Vec<ParquetSchemaTreeNode>),
}

/// Convert [`parquet::schema::types::Type`] to a tree structure.
pub fn build_parquet_schema_tree(
    ty: &parquet::schema::types::Type,
    leave_id: &mut usize,
) -> ParquetSchemaTreeNode {
    match ty {
        parquet::schema::types::Type::PrimitiveType { .. } => {
            let res = ParquetSchemaTreeNode::Leaf(*leave_id);
            *leave_id += 1;
            res
        }
        parquet::schema::types::Type::GroupType { fields, .. } => {
            let mut children = Vec::with_capacity(fields.len());
            for field in fields.iter() {
                children.push(build_parquet_schema_tree(field, leave_id));
            }
            ParquetSchemaTreeNode::Inner(children)
        }
    }
}

/// Traverse the schema tree by `path` to collect the leaves' ids.
pub fn traverse_parquet_schema_tree(
    node: &ParquetSchemaTreeNode,
    path: &[FieldIndex],
    leaves: &mut Vec<usize>,
) {
    match node {
        ParquetSchemaTreeNode::Leaf(id) => {
            leaves.push(*id);
        }
        ParquetSchemaTreeNode::Inner(children) => {
            if path.is_empty() {
                // All children should be included.
                for child in children.iter() {
                    traverse_parquet_schema_tree(child, path, leaves);
                }
            } else {
                let child = path[0];
                traverse_parquet_schema_tree(&children[child], &path[1..], leaves);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use common_expression::types::NumberDataType;
    use common_expression::TableDataType;
    use common_expression::TableField;
    use common_expression::TableSchema;
    use parquet::arrow::arrow_to_parquet_schema;

    use crate::parquet_rs::build_parquet_schema_tree;
    use crate::parquet_rs::ParquetSchemaTreeNode;

    #[test]
    fn test_build_parquet_schema_tree() {
        // Test schema (6 physical columns):
        // a: Int32,            (leave id: 0, path: [0])
        // b: Tuple (
        //    c: Int32,         (leave id: 1, path: [1, 0])
        //    d: Tuple (
        //        e: Int32,     (leave id: 2, path: [1, 1, 0])
        //        f: String,    (leave id: 3, path: [1, 1, 1])
        //    ),
        //    g: String,        (leave id: 4, path: [1, 2])
        // )
        // h: String,           (leave id: 5, path: [2])
        let schema = TableSchema::new(vec![
            TableField::new("a", TableDataType::Number(NumberDataType::Int32)),
            TableField::new("b", TableDataType::Tuple {
                fields_name: vec!["c".to_string(), "d".to_string(), "g".to_string()],
                fields_type: vec![
                    TableDataType::Number(NumberDataType::Int32),
                    TableDataType::Tuple {
                        fields_name: vec!["e".to_string(), "f".to_string()],
                        fields_type: vec![
                            TableDataType::Number(NumberDataType::Int32),
                            TableDataType::String,
                        ],
                    },
                    TableDataType::String,
                ],
            }),
            TableField::new("h", TableDataType::String),
        ]);
        let arrow_fields = schema.to_arrow().fields;
        let arrow_schema = arrow_schema::Schema::new(
            arrow_fields
                .into_iter()
                .map(arrow_schema::Field::from)
                .collect::<Vec<_>>(),
        );
        let schema_desc = arrow_to_parquet_schema(&arrow_schema).unwrap();
        let mut leave_id = 0;
        let tree = build_parquet_schema_tree(schema_desc.root_schema(), &mut leave_id);
        assert_eq!(leave_id, 6);
        let expected_tree = ParquetSchemaTreeNode::Inner(vec![
            ParquetSchemaTreeNode::Leaf(0),
            ParquetSchemaTreeNode::Inner(vec![
                ParquetSchemaTreeNode::Leaf(1),
                ParquetSchemaTreeNode::Inner(vec![
                    ParquetSchemaTreeNode::Leaf(2),
                    ParquetSchemaTreeNode::Leaf(3),
                ]),
                ParquetSchemaTreeNode::Leaf(4),
            ]),
            ParquetSchemaTreeNode::Leaf(5),
        ]);
        assert_eq!(tree, expected_tree);
    }
}
