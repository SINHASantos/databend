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

use databend_common_exception::Result;
use databend_common_expression::types::number::NumberColumnBuilder;
use databend_common_expression::types::BinaryType;
use databend_common_expression::types::Bitmap;
use databend_common_expression::types::DataType;
use databend_common_expression::types::NumberDataType;
use databend_common_expression::types::UnaryType;
use databend_common_expression::with_number_mapped_type;
use databend_common_expression::AggrStateRegistry;
use databend_common_expression::AggrStateType;
use databend_common_expression::BlockEntry;
use databend_common_expression::ColumnBuilder;
use databend_common_expression::ProjectedBlock;
use databend_common_expression::Scalar;
use databend_common_expression::StateSerdeItem;

use super::aggregate_distinct_state::AggregateDistinctNumberState;
use super::aggregate_distinct_state::AggregateDistinctState;
use super::aggregate_distinct_state::AggregateDistinctStringState;
use super::aggregate_distinct_state::AggregateUniqStringState;
use super::aggregate_distinct_state::DistinctStateFunc;
use super::aggregate_function::AggregateFunction;
use super::aggregate_function_factory::AggregateFunctionCreator;
use super::aggregate_function_factory::AggregateFunctionDescription;
use super::aggregate_function_factory::AggregateFunctionSortDesc;
use super::aggregate_function_factory::CombinatorDescription;
use super::aggregator_common::assert_variadic_arguments;
use super::AggregateCountFunction;
use crate::aggregates::AggrState;
use crate::aggregates::AggrStateLoc;
use crate::aggregates::StateAddr;

#[derive(Clone)]
pub struct AggregateDistinctCombinator<State> {
    name: String,

    nested_name: String,
    arguments: Vec<DataType>,
    nested: Arc<dyn AggregateFunction>,
    _state: PhantomData<State>,
}

impl<State> AggregateDistinctCombinator<State> {
    fn get_state(place: AggrState) -> &mut State {
        place
            .addr
            .next(place.loc[0].into_custom().unwrap().1)
            .get::<State>()
    }

    fn set_state(place: AggrState, state: State) {
        place
            .addr
            .next(place.loc[0].into_custom().unwrap().1)
            .write_state(state);
    }
}

impl<State> AggregateFunction for AggregateDistinctCombinator<State>
where State: DistinctStateFunc
{
    fn name(&self) -> &str {
        &self.name
    }

    fn return_type(&self) -> Result<DataType> {
        self.nested.return_type()
    }

    fn init_state(&self, place: AggrState) {
        Self::set_state(place, State::new());
        self.nested.init_state(place.remove_first_loc());
    }

    fn register_state(&self, registry: &mut AggrStateRegistry) {
        registry.register(AggrStateType::Custom(Layout::new::<State>()));
        self.nested.register_state(registry);
    }

    fn accumulate(
        &self,
        place: AggrState,
        columns: ProjectedBlock,
        validity: Option<&Bitmap>,
        input_rows: usize,
    ) -> Result<()> {
        let state = Self::get_state(place);
        state.batch_add(columns, validity, input_rows)
    }

    fn accumulate_row(&self, place: AggrState, columns: ProjectedBlock, row: usize) -> Result<()> {
        let state = Self::get_state(place);
        state.add(columns, row)
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
            let state = Self::get_state(AggrState::new(*place, loc));
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
                let state = Self::get_state(AggrState::new(*place, loc));
                let rhs = State::deserialize(&mut data)?;
                state.merge(&rhs)?;
            }
        } else {
            for (place, mut data) in iter {
                let state = Self::get_state(AggrState::new(*place, loc));
                let rhs = State::deserialize(&mut data)?;
                state.merge(&rhs)?;
            }
        }
        Ok(())
    }

    fn merge_states(&self, place: AggrState, rhs: AggrState) -> Result<()> {
        let state = Self::get_state(place);
        let other = Self::get_state(rhs);
        state.merge(other)
    }

    fn merge_result(&self, place: AggrState, builder: &mut ColumnBuilder) -> Result<()> {
        let state = Self::get_state(place);
        let nested_place = place.remove_first_loc();

        // faster path for count
        if self.nested.name() == "AggregateCountFunction" {
            match builder {
                ColumnBuilder::Number(NumberColumnBuilder::UInt64(builder)) => {
                    builder.push(state.len() as u64);
                }
                _ => unreachable!(),
            }
            Ok(())
        } else {
            if state.is_empty() {
                return self.nested.merge_result(nested_place, builder);
            }
            let entries = &state.build_entries(&self.arguments).unwrap();
            self.nested
                .accumulate(nested_place, entries.into(), None, state.len())?;
            // merge_result
            self.nested.merge_result(nested_place, builder)
        }
    }

    fn need_manual_drop_state(&self) -> bool {
        true
    }

    unsafe fn drop_state(&self, place: AggrState) {
        let state = Self::get_state(place);
        std::ptr::drop_in_place(state);

        if self.nested.need_manual_drop_state() {
            self.nested.drop_state(place.remove_first_loc());
        }
    }

    fn get_if_condition(&self, columns: ProjectedBlock) -> Option<Bitmap> {
        self.nested.get_if_condition(columns)
    }
}

impl<State> fmt::Display for AggregateDistinctCombinator<State> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.nested_name.as_str() {
            "uniq" => write!(f, "uniq"),
            _ => write!(f, "{}_distinct", self.nested_name),
        }
    }
}

pub fn aggregate_combinator_distinct_desc() -> CombinatorDescription {
    CombinatorDescription::creator(Box::new(try_create))
}

pub fn aggregate_combinator_uniq_desc() -> AggregateFunctionDescription {
    let features = super::aggregate_function_factory::AggregateFunctionFeatures {
        returns_default_when_only_null: true,
        ..Default::default()
    };
    AggregateFunctionDescription::creator_with_features(Box::new(try_create_uniq), features)
}

pub fn try_create_uniq(
    nested_name: &str,
    params: Vec<Scalar>,
    arguments: Vec<DataType>,
    sort_descs: Vec<AggregateFunctionSortDesc>,
) -> Result<Arc<dyn AggregateFunction>> {
    let creator: AggregateFunctionCreator = Box::new(AggregateCountFunction::try_create);
    try_create(nested_name, params, arguments, sort_descs, &creator)
}

pub fn try_create(
    nested_name: &str,
    params: Vec<Scalar>,
    arguments: Vec<DataType>,
    sort_descs: Vec<AggregateFunctionSortDesc>,
    nested_creator: &AggregateFunctionCreator,
) -> Result<Arc<dyn AggregateFunction>> {
    let name = format!("DistinctCombinator({})", nested_name);
    assert_variadic_arguments(&name, arguments.len(), (1, 32))?;

    let nested_arguments = match nested_name {
        "count" | "uniq" => vec![],
        _ => arguments.clone(),
    };
    let nested = nested_creator(nested_name, params, nested_arguments, sort_descs)?;

    if arguments.len() == 1 {
        match &arguments[0] {
            DataType::Number(ty) => with_number_mapped_type!(|NUM_TYPE| match ty {
                NumberDataType::NUM_TYPE => {
                    return Ok(Arc::new(AggregateDistinctCombinator::<
                        AggregateDistinctNumberState<NUM_TYPE>,
                    > {
                        nested_name: nested_name.to_owned(),
                        arguments,
                        nested,
                        name,
                        _state: PhantomData,
                    }));
                }
            }),
            DataType::String => {
                return match nested_name {
                    "count" | "uniq" => Ok(Arc::new(AggregateDistinctCombinator::<
                        AggregateUniqStringState,
                    > {
                        name,
                        arguments,
                        nested,
                        nested_name: nested_name.to_owned(),
                        _state: PhantomData,
                    })),
                    _ => Ok(Arc::new(AggregateDistinctCombinator::<
                        AggregateDistinctStringState,
                    > {
                        nested_name: nested_name.to_owned(),
                        arguments,
                        nested,
                        name,
                        _state: PhantomData,
                    })),
                };
            }
            _ => {}
        }
    }
    Ok(Arc::new(AggregateDistinctCombinator::<
        AggregateDistinctState,
    > {
        nested_name: nested_name.to_owned(),
        arguments,
        nested,
        name,
        _state: PhantomData,
    }))
}
