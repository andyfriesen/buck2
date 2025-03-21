/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::iter;

use buck2_node::attrs::attr_type::tuple::TupleAttrType;
use buck2_node::attrs::attr_type::tuple::TupleLiteral;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use dupe::IterDupedExt;
use gazebo::prelude::SliceExt;
use starlark::typing::Ty;
use starlark::values::list::ListRef;
use starlark::values::tuple::TupleRef;
use starlark::values::Value;

use crate::attrs::coerce::attr_type::AttrTypeExt;
use crate::attrs::coerce::error::CoercionError;
use crate::attrs::coerce::AttrTypeCoerce;

impl AttrTypeCoerce for TupleAttrType {
    fn coerce_item(
        &self,
        configurable: AttrIsConfigurable,
        ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> anyhow::Result<CoercedAttr> {
        let coerce = |value, items: &[Value]| {
            // Use comparison rather than equality below. If the tuple is too short,
            // it is implicitly extended using None.
            if items.len() <= self.xs.len() {
                let mut res = Vec::with_capacity(self.xs.len());
                for (c, v) in self
                    .xs
                    .iter()
                    .zip(items.iter().duped().chain(iter::repeat(Value::new_none())))
                {
                    res.push(c.coerce(configurable, ctx, v)?);
                }
                Ok(CoercedAttr::Tuple(TupleLiteral(ctx.intern_list(res))))
            } else {
                Err(anyhow::anyhow!(CoercionError::type_error(
                    &format!("Tuple of at most length {}", self.xs.len()),
                    value
                )))
            }
        };
        if let Some(list) = TupleRef::from_value(value) {
            coerce(value, list.content())
        } else if let Some(list) = ListRef::from_value(value) {
            coerce(value, list.content())
        } else {
            Err(anyhow::anyhow!(CoercionError::type_error(
                TupleRef::TYPE,
                value,
            )))
        }
    }

    fn starlark_type(&self) -> Ty {
        Ty::tuple(self.xs.map(|x| x.starlark_type()))
    }
}
