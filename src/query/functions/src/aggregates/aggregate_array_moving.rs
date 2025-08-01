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

use std::alloc::Layout;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use borsh::BorshDeserialize;
use borsh::BorshSerialize;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::types::decimal::*;
use databend_common_expression::types::i256;
use databend_common_expression::types::number::Number;
use databend_common_expression::types::AccessType;
use databend_common_expression::types::ArgType;
use databend_common_expression::types::BinaryType;
use databend_common_expression::types::Bitmap;
use databend_common_expression::types::Buffer;
use databend_common_expression::types::DataType;
use databend_common_expression::types::Float64Type;
use databend_common_expression::types::Int8Type;
use databend_common_expression::types::NumberDataType;
use databend_common_expression::types::NumberType;
use databend_common_expression::types::UnaryType;
use databend_common_expression::types::F64;
use databend_common_expression::utils::arithmetics_type::ResultTypeOfUnary;
use databend_common_expression::with_decimal_mapped_type;
use databend_common_expression::with_number_mapped_type;
use databend_common_expression::AggrStateRegistry;
use databend_common_expression::AggrStateType;
use databend_common_expression::BlockEntry;
use databend_common_expression::Column;
use databend_common_expression::ColumnBuilder;
use databend_common_expression::ProjectedBlock;
use databend_common_expression::Scalar;
use databend_common_expression::ScalarRef;
use databend_common_expression::StateSerdeItem;
use num_traits::AsPrimitive;

use super::aggregate_function::AggregateFunction;
use super::aggregate_function::AggregateFunctionRef;
use super::aggregate_function_factory::AggregateFunctionDescription;
use super::aggregate_function_factory::AggregateFunctionSortDesc;
use super::extract_number_param;
use super::StateAddr;
use crate::aggregates::aggregate_sum::SumState;
use crate::aggregates::assert_unary_arguments;
use crate::aggregates::assert_variadic_params;
use crate::aggregates::AggrState;
use crate::aggregates::AggrStateLoc;

#[derive(Default, Debug, BorshDeserialize, BorshSerialize)]
pub struct NumberArrayMovingSumState<T, TSum> {
    values: Vec<T>,
    #[borsh(skip)]
    _t: PhantomData<TSum>,
}

impl<T, TSum> SumState for NumberArrayMovingSumState<T, TSum>
where
    T: Number + AsPrimitive<TSum> + BorshSerialize + BorshDeserialize,
    TSum: Number + AsPrimitive<f64> + std::ops::AddAssign + std::ops::SubAssign,
{
    fn accumulate_row(&mut self, entry: &BlockEntry, row: usize) -> Result<()> {
        let column = &entry.to_column();
        let buffer = match column {
            Column::Null { .. } => {
                self.values.push(T::default());
                return Ok(());
            }
            Column::Nullable(box nullable_column) => {
                NumberType::<T>::try_downcast_column(&nullable_column.column).unwrap()
            }
            _ => NumberType::<T>::try_downcast_column(column).unwrap(),
        };
        self.values.push(buffer[row]);
        Ok(())
    }

    fn accumulate(&mut self, entry: &BlockEntry, validity: Option<&Bitmap>) -> Result<()> {
        let column = match entry.data_type() {
            DataType::Null => {
                for _ in 0..entry.len() {
                    self.values.push(T::default());
                }
                return Ok(());
            }
            _ => entry
                .clone()
                .remove_nullable()
                .downcast::<NumberType<T>>()
                .unwrap(),
        };
        if let Some(validity) = validity {
            column.iter().zip(validity.iter()).for_each(|(v, b)| {
                if b {
                    self.values.push(v);
                } else {
                    self.values.push(T::default());
                }
            });
        } else {
            column.iter().for_each(|v| {
                self.values.push(v);
            });
        }
        Ok(())
    }

    fn accumulate_keys(
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        entry: &BlockEntry,
    ) -> Result<()> {
        let buffer = match entry.to_column() {
            Column::Null { len } => Buffer::from(vec![T::default(); len]),
            Column::Nullable(box nullable_column) => {
                NumberType::<T>::try_downcast_column(&nullable_column.column).unwrap()
            }
            column => NumberType::<T>::try_downcast_column(&column).unwrap(),
        };
        buffer.iter().zip(places.iter()).for_each(|(c, place)| {
            let state = AggrState::new(*place, loc).get::<Self>();
            state.values.push(*c);
        });
        Ok(())
    }

    #[inline(always)]
    fn merge(&mut self, other: &Self) -> Result<()> {
        self.values.extend_from_slice(&other.values);
        Ok(())
    }

    fn merge_result(
        &mut self,
        builder: &mut ColumnBuilder,
        window_size: &Option<usize>,
    ) -> Result<()> {
        let window_size = match window_size {
            Some(window_size) => *window_size,
            None => self.values.len(),
        };

        let mut sum = TSum::default();
        let mut sum_values: Vec<TSum> = Vec::with_capacity(self.values.len());
        for (i, value) in self.values.iter().enumerate() {
            sum += value.as_();
            if i >= window_size {
                sum -= self.values[i - window_size].as_();
            }
            sum_values.push(sum);
        }

        let inner_col = NumberType::<TSum>::upcast_column(sum_values.into());
        let array_value = ScalarRef::Array(inner_col);
        builder.push(array_value);

        Ok(())
    }

    fn merge_avg_result(
        &mut self,
        builder: &mut ColumnBuilder,
        _count: u64,
        _scale_add: u8,
        window_size: &Option<usize>,
    ) -> Result<()> {
        let window_size = match window_size {
            Some(window_size) => *window_size,
            None => self.values.len(),
        };

        let mut sum = TSum::default();
        let mut avg_values: Vec<F64> = Vec::with_capacity(self.values.len());
        for (i, value) in self.values.iter().enumerate() {
            sum += value.as_();
            if i >= window_size {
                sum -= self.values[i - window_size].as_();
            }
            let avg_val = sum.as_() / (window_size as f64);
            avg_values.push(avg_val.into());
        }

        let inner_col = Float64Type::upcast_column(avg_values.into());
        let array_value = ScalarRef::Array(inner_col);
        builder.push(array_value);

        Ok(())
    }
}

#[derive(Default, BorshDeserialize, BorshSerialize)]
pub struct DecimalArrayMovingSumState<T> {
    pub values: Vec<T>,
}

impl<T> DecimalArrayMovingSumState<T>
where T: Decimal
        + std::ops::AddAssign
        + BorshSerialize
        + BorshDeserialize
        + Copy
        + Clone
        + std::fmt::Debug
        + std::cmp::PartialOrd
{
    #[inline]
    pub fn check_over_flow(&self, value: T) -> Result<()> {
        if value > T::DECIMAL_MAX || value < T::DECIMAL_MIN {
            return Err(ErrorCode::Overflow(format!(
                "Decimal overflow: {} not in [{}, {}]",
                value,
                T::DECIMAL_MIN,
                T::DECIMAL_MAX,
            )));
        }
        Ok(())
    }
}

impl<T> SumState for DecimalArrayMovingSumState<T>
where T: Decimal
        + std::ops::AddAssign
        + std::ops::SubAssign
        + BorshSerialize
        + BorshDeserialize
        + Copy
        + Clone
        + std::fmt::Debug
        + std::cmp::PartialOrd
{
    fn accumulate_row(&mut self, entry: &BlockEntry, row: usize) -> Result<()> {
        let column = &entry.to_column();
        let buffer = match column {
            Column::Null { .. } => {
                self.values.push(T::default());
                return Ok(());
            }
            Column::Nullable(box nullable_column) => {
                T::try_downcast_column(&nullable_column.column).unwrap().0
            }
            _ => T::try_downcast_column(column).unwrap().0,
        };
        self.values.push(buffer[row]);
        Ok(())
    }

    fn accumulate(&mut self, entry: &BlockEntry, validity: Option<&Bitmap>) -> Result<()> {
        let column = &entry.to_column();
        let buffer = match column {
            Column::Null { len } => {
                for _ in 0..*len {
                    self.values.push(T::default());
                }
                return Ok(());
            }
            Column::Nullable(box nullable_column) => {
                T::try_downcast_column(&nullable_column.column).unwrap().0
            }
            _ => T::try_downcast_column(column).unwrap().0,
        };
        match validity {
            Some(validity) => {
                for (i, v) in validity.iter().enumerate() {
                    if v {
                        self.values.push(buffer[i]);
                    } else {
                        self.values.push(T::default());
                    }
                }
            }
            None => {
                for v in buffer.iter() {
                    self.values.push(*v);
                }
            }
        }
        Ok(())
    }

    fn accumulate_keys(
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        entry: &BlockEntry,
    ) -> Result<()> {
        let buffer = match entry.to_column() {
            Column::Null { len } => Buffer::from(vec![T::default(); len]),
            Column::Nullable(box nullable_column) => {
                T::try_downcast_column(&nullable_column.column).unwrap().0
            }
            column => T::try_downcast_column(&column).unwrap().0,
        };
        buffer.iter().zip(places.iter()).for_each(|(c, place)| {
            let state = AggrState::new(*place, loc).get::<Self>();
            state.values.push(*c);
        });
        Ok(())
    }

    #[inline(always)]
    fn merge(&mut self, other: &Self) -> Result<()> {
        self.values.extend_from_slice(&other.values);
        Ok(())
    }

    fn merge_result(
        &mut self,
        builder: &mut ColumnBuilder,
        window_size: &Option<usize>,
    ) -> Result<()> {
        let window_size = match window_size {
            Some(window_size) => *window_size,
            None => self.values.len(),
        };

        let mut sum = T::default();
        let mut sum_values: Vec<T> = Vec::with_capacity(self.values.len());
        for (i, value) in self.values.iter().enumerate() {
            sum += *value;
            self.check_over_flow(sum)?;
            if i >= window_size {
                sum -= self.values[i - window_size];
            }
            sum_values.push(sum);
        }

        let data_type = builder.data_type();
        let inner_type = data_type.as_array().unwrap();
        let size = inner_type.as_decimal().unwrap();

        let inner_col = T::upcast_column(sum_values.into(), *size);
        builder.push(ScalarRef::Array(inner_col));

        Ok(())
    }

    fn merge_avg_result(
        &mut self,
        builder: &mut ColumnBuilder,
        _count: u64,
        scale_add: u8,
        window_size: &Option<usize>,
    ) -> Result<()> {
        let window_size = match window_size {
            Some(window_size) => *window_size,
            None => self.values.len(),
        };

        let mut sum = T::default();
        let mut avg_values: Vec<T> = Vec::with_capacity(self.values.len());
        for (i, value) in self.values.iter().enumerate() {
            sum += *value;
            self.check_over_flow(sum)?;
            if i >= window_size {
                sum -= self.values[i - window_size];
            }
            let avg_val = match sum
                .checked_mul(T::e(scale_add))
                .and_then(|v| v.checked_div(T::from_i128(window_size as u64)))
            {
                Some(value) => value,
                None => {
                    return Err(ErrorCode::Overflow(format!(
                        "Decimal overflow: {} mul {}",
                        sum,
                        T::e(scale_add)
                    )));
                }
            };
            avg_values.push(avg_val);
        }

        let data_type = builder.data_type();
        let inner_type = data_type.as_array().unwrap();
        let decimal_size = inner_type.as_decimal().unwrap();

        let inner_col = T::upcast_column(avg_values.into(), *decimal_size);
        let array_value = ScalarRef::Array(inner_col);
        builder.push(array_value);

        Ok(())
    }
}

#[derive(Clone)]
pub struct AggregateArrayMovingAvgFunction<State> {
    display_name: String,
    window_size: Option<usize>,
    sum_t: PhantomData<State>,
    return_type: DataType,
    scale_add: u8,
}

impl<State> AggregateFunction for AggregateArrayMovingAvgFunction<State>
where State: SumState
{
    fn name(&self) -> &str {
        "AggregateArrayMovingAvgFunction"
    }

    fn return_type(&self) -> Result<DataType> {
        Ok(self.return_type.clone())
    }

    fn init_state(&self, place: AggrState) {
        place.write(State::default);
    }

    fn register_state(&self, registry: &mut AggrStateRegistry) {
        registry.register(AggrStateType::Custom(Layout::new::<State>()));
    }

    fn accumulate(
        &self,
        place: AggrState,
        columns: ProjectedBlock,
        validity: Option<&Bitmap>,
        _input_rows: usize,
    ) -> Result<()> {
        let state = place.get::<State>();
        state.accumulate(&columns[0], validity)
    }

    fn accumulate_keys(
        &self,
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        columns: ProjectedBlock,
        _input_rows: usize,
    ) -> Result<()> {
        State::accumulate_keys(places, loc, &columns[0])
    }

    fn accumulate_row(&self, place: AggrState, columns: ProjectedBlock, row: usize) -> Result<()> {
        let state = place.get::<State>();
        state.accumulate_row(&columns[0], row)
    }

    fn serialize_type(&self) -> Vec<StateSerdeItem> {
        vec![StateSerdeItem::Binary(None)]
    }

    fn batch_serialize(
        &self,
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        builders: &mut [ColumnBuilder],
    ) -> Result<()> {
        let binary_builder = builders[0].as_binary_mut().unwrap();
        for place in places {
            let state = AggrState::new(*place, loc).get::<State>();
            state.serialize(&mut binary_builder.data)?;
            binary_builder.commit_row();
        }
        Ok(())
    }

    fn batch_merge(
        &self,
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        state: &BlockEntry,
        filter: Option<&Bitmap>,
    ) -> Result<()> {
        let view = state.downcast::<UnaryType<BinaryType>>().unwrap();
        let iter = places.iter().zip(view.iter());

        if let Some(filter) = filter {
            for (place, mut data) in iter.zip(filter.iter()).filter_map(|(v, b)| b.then_some(v)) {
                let state = AggrState::new(*place, loc).get::<State>();
                let rhs = State::deserialize_reader(&mut data)?;
                state.merge(&rhs)?;
            }
        } else {
            for (place, mut data) in iter {
                let state = AggrState::new(*place, loc).get::<State>();
                let rhs = State::deserialize_reader(&mut data)?;
                state.merge(&rhs)?;
            }
        }
        Ok(())
    }

    fn merge_states(&self, place: AggrState, rhs: AggrState) -> Result<()> {
        let state = place.get::<State>();
        let other = rhs.get::<State>();
        state.merge(other)
    }

    fn merge_result(&self, place: AggrState, builder: &mut ColumnBuilder) -> Result<()> {
        let state = place.get::<State>();
        state.merge_avg_result(builder, 0_u64, self.scale_add, &self.window_size)
    }

    fn need_manual_drop_state(&self) -> bool {
        true
    }

    unsafe fn drop_state(&self, place: AggrState) {
        let state = place.get::<State>();
        std::ptr::drop_in_place(state);
    }
}

impl<State> fmt::Display for AggregateArrayMovingAvgFunction<State> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.display_name)
    }
}

impl<State> AggregateArrayMovingAvgFunction<State>
where State: SumState
{
    pub fn try_create(
        display_name: &str,
        params: Vec<Scalar>,
        return_type: DataType,
        scale_add: u8,
    ) -> Result<AggregateFunctionRef> {
        let window_size = if params.len() == 1 {
            let window_size: u64 = extract_number_param(params[0].clone())?;
            Some(window_size as usize)
        } else {
            None
        };

        Ok(Arc::new(Self {
            display_name: display_name.to_owned(),
            window_size,
            sum_t: PhantomData,
            return_type,
            scale_add,
        }))
    }
}

pub fn try_create_aggregate_array_moving_avg_function(
    display_name: &str,
    params: Vec<Scalar>,
    arguments: Vec<DataType>,
    _sort_descs: Vec<AggregateFunctionSortDesc>,
) -> Result<AggregateFunctionRef> {
    assert_unary_arguments(display_name, arguments.len())?;
    assert_variadic_params(display_name, params.len(), (0, 1))?;

    let data_type = if arguments[0].is_null() {
        Int8Type::data_type()
    } else {
        arguments[0].remove_nullable()
    };
    with_number_mapped_type!(|NUM_TYPE| match &data_type {
        DataType::Number(NumberDataType::NUM_TYPE) => {
            type TSum = <NUM_TYPE as ResultTypeOfUnary>::Sum;
            type State = NumberArrayMovingSumState<NUM_TYPE, TSum>;
            AggregateArrayMovingAvgFunction::<State>::try_create(
                display_name,
                params,
                DataType::Array(Box::new(Float64Type::data_type())),
                0,
            )
        }
        DataType::Decimal(s) => {
            with_decimal_mapped_type!(|DECIMAL| match s.data_kind() {
                DecimalDataKind::DECIMAL => {
                    let decimal_size =
                        DecimalSize::new_unchecked(DECIMAL::MAX_PRECISION, s.scale().max(4));

                    AggregateArrayMovingAvgFunction::<DecimalArrayMovingSumState<DECIMAL>>::try_create(
                    display_name,
                    params,
                    DataType::Array(Box::new(DataType::Decimal(decimal_size))),
                    decimal_size.scale() - s.scale(),
                )
                }
            })
        }
        _ => Err(ErrorCode::BadDataValueType(format!(
            "AggregateArrayMovingAvgFunction does not support type '{:?}'",
            arguments[0]
        ))),
    })
}

pub fn aggregate_array_moving_avg_function_desc() -> AggregateFunctionDescription {
    AggregateFunctionDescription::creator(Box::new(try_create_aggregate_array_moving_avg_function))
}

#[derive(Clone)]
pub struct AggregateArrayMovingSumFunction<State> {
    display_name: String,
    window_size: Option<usize>,
    sum_t: PhantomData<State>,
    return_type: DataType,
}

impl<State> AggregateFunction for AggregateArrayMovingSumFunction<State>
where State: SumState
{
    fn name(&self) -> &str {
        "AggregateArrayMovingSumFunction"
    }

    fn return_type(&self) -> Result<DataType> {
        Ok(self.return_type.clone())
    }

    fn init_state(&self, place: AggrState) {
        place.write(State::default);
    }

    fn register_state(&self, registry: &mut AggrStateRegistry) {
        registry.register(AggrStateType::Custom(Layout::new::<State>()));
    }

    fn accumulate(
        &self,
        place: AggrState,
        columns: ProjectedBlock,
        validity: Option<&Bitmap>,
        _input_rows: usize,
    ) -> Result<()> {
        let state = place.get::<State>();
        state.accumulate(&columns[0], validity)
    }

    fn accumulate_keys(
        &self,
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        columns: ProjectedBlock,
        _input_rows: usize,
    ) -> Result<()> {
        State::accumulate_keys(places, loc, &columns[0])
    }

    fn accumulate_row(&self, place: AggrState, columns: ProjectedBlock, row: usize) -> Result<()> {
        let state = place.get::<State>();
        state.accumulate_row(&columns[0], row)
    }

    fn serialize_type(&self) -> Vec<StateSerdeItem> {
        vec![StateSerdeItem::Binary(None)]
    }

    fn batch_serialize(
        &self,
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        builders: &mut [ColumnBuilder],
    ) -> Result<()> {
        let binary_builder = builders[0].as_binary_mut().unwrap();
        for place in places {
            let state = AggrState::new(*place, loc).get::<State>();
            state.serialize(&mut binary_builder.data)?;
            binary_builder.commit_row();
        }
        Ok(())
    }

    fn batch_merge(
        &self,
        places: &[StateAddr],
        loc: &[AggrStateLoc],
        state: &BlockEntry,
        filter: Option<&Bitmap>,
    ) -> Result<()> {
        let view = state.downcast::<UnaryType<BinaryType>>().unwrap();
        let iter = places.iter().zip(view.iter());

        if let Some(filter) = filter {
            for (place, mut data) in iter.zip(filter.iter()).filter_map(|(v, b)| b.then_some(v)) {
                let state = AggrState::new(*place, loc).get::<State>();
                let rhs = State::deserialize_reader(&mut data)?;
                state.merge(&rhs)?;
            }
        } else {
            for (place, mut data) in iter {
                let state = AggrState::new(*place, loc).get::<State>();
                let rhs = State::deserialize_reader(&mut data)?;
                state.merge(&rhs)?;
            }
        }
        Ok(())
    }

    fn merge_states(&self, place: AggrState, rhs: AggrState) -> Result<()> {
        let state = place.get::<State>();
        let other = rhs.get::<State>();
        state.merge(other)
    }

    fn merge_result(&self, place: AggrState, builder: &mut ColumnBuilder) -> Result<()> {
        let state = place.get::<State>();
        state.merge_result(builder, &self.window_size)
    }

    fn need_manual_drop_state(&self) -> bool {
        true
    }

    unsafe fn drop_state(&self, place: AggrState) {
        let state = place.get::<State>();
        std::ptr::drop_in_place(state);
    }
}

impl<State> fmt::Display for AggregateArrayMovingSumFunction<State> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.display_name)
    }
}

impl<State> AggregateArrayMovingSumFunction<State>
where State: SumState
{
    pub fn try_create(
        display_name: &str,
        params: Vec<Scalar>,
        return_type: DataType,
    ) -> Result<AggregateFunctionRef> {
        let window_size = if params.len() == 1 {
            let window_size: u64 = extract_number_param(params[0].clone())?;
            Some(window_size as usize)
        } else {
            None
        };

        Ok(Arc::new(Self {
            display_name: display_name.to_owned(),
            window_size,
            sum_t: PhantomData,
            return_type,
        }))
    }
}

pub fn try_create_aggregate_array_moving_sum_function(
    display_name: &str,
    params: Vec<Scalar>,
    arguments: Vec<DataType>,
    _sort_descs: Vec<AggregateFunctionSortDesc>,
) -> Result<AggregateFunctionRef> {
    assert_unary_arguments(display_name, arguments.len())?;
    assert_variadic_params(display_name, params.len(), (0, 1))?;

    let data_type = if arguments[0].is_null() {
        Int8Type::data_type()
    } else {
        arguments[0].remove_nullable()
    };
    with_number_mapped_type!(|NUM_TYPE| match &data_type {
        DataType::Number(NumberDataType::NUM_TYPE) => {
            type TSum = <NUM_TYPE as ResultTypeOfUnary>::Sum;
            type State = NumberArrayMovingSumState<NUM_TYPE, TSum>;
            AggregateArrayMovingSumFunction::<State>::try_create(
                display_name,
                params,
                DataType::Array(Box::new(NumberType::<TSum>::data_type())),
            )
        }
        DataType::Decimal(s) => {
            with_decimal_mapped_type!(|DECIMAL| match s.data_kind() {
                DecimalDataKind::DECIMAL => {
                    let decimal_size =
                        DecimalSize::new_unchecked(DECIMAL::MAX_PRECISION, s.scale());

                    AggregateArrayMovingSumFunction::<DecimalArrayMovingSumState<DECIMAL>>::try_create(
                    display_name,
                    params,
                    DataType::Array(Box::new(DataType::Decimal(decimal_size))),
                )
                }
            })
        }
        _ => Err(ErrorCode::BadDataValueType(format!(
            "AggregateArrayMovingSumFunction does not support type '{:?}'",
            arguments[0]
        ))),
    })
}

pub fn aggregate_array_moving_sum_function_desc() -> AggregateFunctionDescription {
    AggregateFunctionDescription::creator(Box::new(try_create_aggregate_array_moving_sum_function))
}
