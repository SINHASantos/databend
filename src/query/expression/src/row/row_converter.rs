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

use std::sync::Arc;

use databend_common_column::types::months_days_micros;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;

use super::fixed;
use super::fixed::FixedLengthEncoding;
use super::variable;
use crate::types::binary::BinaryColumn;
use crate::types::binary::BinaryColumnBuilder;
use crate::types::decimal::DecimalColumn;
use crate::types::i256;
use crate::types::AccessType;
use crate::types::DataType;
use crate::types::DecimalDataKind;
use crate::types::DecimalView;
use crate::types::NumberColumn;
use crate::types::NumberDataType;
use crate::with_decimal_mapped_type;
use crate::with_number_mapped_type;
use crate::with_number_type;
use crate::Column;
use crate::SortField;

/// Convert column-oriented data into comparable row-oriented data.
///
/// **NOTE**: currently, Variant is treat as String.
#[derive(Debug)]
pub struct RowConverter {
    fields: Arc<[SortField]>,
}

impl RowConverter {
    pub fn new(fields: Vec<SortField>) -> Result<Self> {
        if !fields.iter().all(|f| Self::support_data_type(&f.data_type)) {
            return Err(ErrorCode::Unimplemented(format!(
                "Row format is not yet support for {:?}",
                fields
            )));
        }

        Ok(Self {
            fields: fields.into(),
        })
    }

    fn support_data_type(d: &DataType) -> bool {
        match d {
            DataType::Null
            | DataType::Boolean
            | DataType::Number(_)
            | DataType::Decimal(_)
            | DataType::Timestamp
            | DataType::Interval
            | DataType::Date
            | DataType::Binary
            | DataType::String
            | DataType::Variant => true,
            DataType::Nullable(inner) => Self::support_data_type(inner.as_ref()),
            _ => false,
        }
    }

    /// Convert columns into [`BinaryColumn`] represented comparable row format.
    pub fn convert_columns(&self, columns: &[Column], num_rows: usize) -> BinaryColumn {
        debug_assert_eq!(columns.len(), self.fields.len());
        debug_assert!(columns
            .iter()
            .zip(self.fields.iter())
            .all(|(col, f)| col.len() == num_rows && col.data_type() == f.data_type));

        let mut builder = self.new_empty_rows(columns, num_rows);
        for (column, field) in columns.iter().zip(self.fields.iter()) {
            encode_column(&mut builder, column, field.asc, field.nulls_first);
        }

        builder.build()
    }

    fn new_empty_rows(&self, cols: &[Column], num_rows: usize) -> BinaryColumnBuilder {
        let mut lengths = vec![0_usize; num_rows];

        for (field, col) in self.fields.iter().zip(cols.iter()) {
            // Both nullable and non-nullable data will be encoded with null sentinel byte.
            let (all_null, validity) = col.validity();
            let ty = field.data_type.remove_nullable();
            match ty {
                DataType::Null => {}
                DataType::Boolean => lengths.iter_mut().for_each(|x| *x += bool::ENCODED_LEN),
                DataType::Number(t) => with_number_mapped_type!(|NUM_TYPE| match t {
                    NumberDataType::NUM_TYPE => {
                        lengths.iter_mut().for_each(|x| *x += NUM_TYPE::ENCODED_LEN)
                    }
                }),
                DataType::Decimal(size) => match size.data_kind() {
                    DecimalDataKind::Decimal64 => {
                        lengths.iter_mut().for_each(|x| *x += i64::ENCODED_LEN)
                    }
                    DecimalDataKind::Decimal128 => {
                        lengths.iter_mut().for_each(|x| *x += i128::ENCODED_LEN)
                    }
                    DecimalDataKind::Decimal256 => {
                        lengths.iter_mut().for_each(|x| *x += i256::ENCODED_LEN)
                    }
                },
                DataType::Timestamp => lengths.iter_mut().for_each(|x| *x += i64::ENCODED_LEN),
                DataType::Interval => lengths
                    .iter_mut()
                    .for_each(|x| *x += months_days_micros::ENCODED_LEN),
                DataType::Date => lengths.iter_mut().for_each(|x| *x += i32::ENCODED_LEN),
                DataType::Binary => {
                    let col = col.remove_nullable();
                    if all_null {
                        lengths.iter_mut().for_each(|x| *x += 1)
                    } else if let Some(validity) = validity {
                        col.as_binary()
                            .unwrap()
                            .iter()
                            .zip(validity.iter())
                            .zip(lengths.iter_mut())
                            .for_each(|((bytes, v), length)| {
                                *length += variable::encoded_len(bytes, !v)
                            })
                    } else {
                        col.as_binary()
                            .unwrap()
                            .iter()
                            .zip(lengths.iter_mut())
                            .for_each(|(bytes, length)| {
                                *length += variable::encoded_len(bytes, false)
                            })
                    }
                }
                DataType::String => {
                    let col = col.remove_nullable();
                    if all_null {
                        lengths.iter_mut().for_each(|x| *x += 1)
                    } else if let Some(validity) = validity {
                        col.as_string()
                            .unwrap()
                            .iter()
                            .zip(validity.iter())
                            .zip(lengths.iter_mut())
                            .for_each(|((str, v), length)| {
                                *length += variable::encoded_len(str.as_bytes(), !v)
                            })
                    } else {
                        col.as_string()
                            .unwrap()
                            .iter()
                            .zip(lengths.iter_mut())
                            .for_each(|(str, length)| {
                                *length += variable::encoded_len(str.as_bytes(), false)
                            })
                    }
                }
                DataType::Variant => {
                    let col = col.remove_nullable();
                    if all_null {
                        lengths.iter_mut().for_each(|x| *x += 1)
                    } else if let Some(validity) = validity {
                        col.as_variant()
                            .unwrap()
                            .iter()
                            .zip(validity.iter())
                            .zip(lengths.iter_mut())
                            .for_each(|((bytes, v), length)| {
                                *length += variable::encoded_len(bytes, !v)
                            })
                    } else {
                        col.as_variant()
                            .unwrap()
                            .iter()
                            .zip(lengths.iter_mut())
                            .for_each(|(bytes, length)| {
                                *length += variable::encoded_len(bytes, false)
                            })
                    }
                }
                _ => unimplemented!(),
            }
        }

        let mut offsets = Vec::with_capacity(num_rows + 1);
        offsets.push(0);

        // Comments from apache/arrow-rs:
        // We initialize the offsets shifted down by one row index.
        //
        // As the rows are appended to the offsets will be incremented to match
        //
        // For example, consider the case of 3 rows of length 3, 4, and 6 respectively.
        // The offsets would be initialized to `0, 0, 3, 7`
        //
        // Writing the first row entirely would yield `0, 3, 3, 7`
        // The second, `0, 3, 7, 7`
        // The third, `0, 3, 7, 13`
        //
        // This would be the final offsets for reading
        //
        // In this way offsets tracks the position during writing whilst eventually serving
        // as identifying the offsets of the written rows
        let mut cur_offset = 0_u64;
        for l in lengths {
            offsets.push(cur_offset);
            cur_offset = cur_offset.checked_add(l as u64).expect("overflow");
        }

        let buffer = vec![0_u8; cur_offset as usize];

        BinaryColumnBuilder::from_data(buffer, offsets)
    }
}

#[inline(always)]
pub(super) fn null_sentinel(nulls_first: bool) -> u8 {
    if nulls_first {
        0
    } else {
        0xFF
    }
}

fn encode_column(out: &mut BinaryColumnBuilder, column: &Column, asc: bool, nulls_first: bool) {
    let validity = column.validity();
    let column = column.remove_nullable();
    match column {
        Column::Null { .. } => {}
        Column::Boolean(col) => fixed::encode(out, col, validity, asc, nulls_first),
        Column::Number(col) => {
            with_number_type!(|NUM_TYPE| match col {
                NumberColumn::NUM_TYPE(c) => {
                    fixed::encode(out, c, validity, asc, nulls_first)
                }
            })
        }
        Column::Decimal(col) => {
            with_decimal_mapped_type!(|F| match col {
                DecimalColumn::F(buffer, size) => {
                    with_decimal_mapped_type!(|T| match size.data_kind() {
                        DecimalDataKind::T => {
                            let iter = DecimalView::<F, T>::iter_column(&buffer);
                            fixed::encode(out, iter, validity, asc, nulls_first)
                        }
                    });
                }
            });
        }
        Column::Timestamp(col) => fixed::encode(out, col, validity, asc, nulls_first),
        Column::Interval(col) => fixed::encode(out, col, validity, asc, nulls_first),
        Column::Date(col) => fixed::encode(out, col, validity, asc, nulls_first),
        Column::Binary(col) => variable::encode(out, col.iter(), validity, asc, nulls_first),
        Column::String(col) => variable::encode(
            out,
            col.iter().map(|s| s.as_bytes()),
            validity,
            asc,
            nulls_first,
        ),
        Column::Variant(col) => variable::encode(out, col.iter(), validity, asc, nulls_first),
        _ => unimplemented!(),
    }
}
