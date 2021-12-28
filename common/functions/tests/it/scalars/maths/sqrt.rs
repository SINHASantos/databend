// Copyright 2021 Datafuse Labs.
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

use common_datavalues::prelude::*;
use common_exception::Result;
use common_functions::scalars::*;
use crate::scalars::scalar_function_test::{ScalarFunctionTest, test_scalar_functions};

#[test]
fn test_sqrt_function() -> Result<()> {
    let tests = vec![
        ScalarFunctionTest {
            name: "sqrt-with-literal",
            nullable: true,
            columns: vec![Series::new(vec![4]).into()],
            expect: Series::new(vec![2_f64]).into(),
            error: "",
        },
        ScalarFunctionTest {
            name: "sqrt-with-series",
            nullable: true,
            columns: vec![Series::new(vec![4, 16, 0]).into()],
            expect: Series::new(vec![2_f64, 4.0, 0.0]).into(),
            error: "",
        },
        ScalarFunctionTest {
            name: "sqrt-with-null",
            nullable: true,
            columns: vec![Series::new(vec![Some(4), None]).into()],
            expect: Series::new(vec![Some(2_f64), None]).into(),
            error: "",
        },
        ScalarFunctionTest {
            name: "sqrt-with-negative",
            nullable: true,
            columns: vec![Series::new(vec![4, -4]).into()],
            expect: Series::new(vec![Some(2_f64), None]).into(),
            error: "",
        },
    ];

    test_scalar_functions(SqrtFunction::try_create("sqrt")?, &tests)
}
