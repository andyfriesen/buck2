/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use anyhow::Context as _;
use buck2_core::metadata_key::MetadataKey;
use buck2_core::metadata_key::MetadataKeyRef;
use buck2_interpreter::types::opaque_metadata::OpaqueMetadata;
use buck2_node::attrs::attr_type::metadata::MetadataAttrType;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use buck2_node::metadata::MetadataMap;
use starlark::values::dict::Dict;
use starlark::values::dict::DictRef;
use starlark::values::string::STRING_TYPE;
use starlark::values::type_repr::StarlarkTypeRepr;
use starlark::values::Value;
use starlark_map::ordered_map::OrderedMap;

use crate::attrs::coerce::attr_type::ty_maybe_select::TyMaybeSelect;
use crate::attrs::coerce::error::CoercionError;
use crate::attrs::coerce::AttrTypeCoerce;

#[derive(Debug, thiserror::Error)]
enum MetadataAttrTypeCoerceError {
    #[error("Metadata attribute is not configurable (internal error)")]
    AttrTypeNotConfigurable,
    #[error(
        "Metadata attribute with key {} is not convertible to JSON: {}",
        .key,
        .value
    )]
    ValueIsNotJson { key: MetadataKey, value: String },
}

impl AttrTypeCoerce for MetadataAttrType {
    fn coerce_item(
        &self,
        configurable: AttrIsConfigurable,
        _ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> anyhow::Result<CoercedAttr> {
        if configurable == AttrIsConfigurable::Yes {
            return Err(MetadataAttrTypeCoerceError::AttrTypeNotConfigurable.into());
        }

        let dict = match DictRef::from_value(value) {
            Some(d) => d,
            None => return Err(CoercionError::type_error(Dict::TYPE, value).into()),
        };

        let mut map = OrderedMap::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            let key = match key.unpack_str() {
                Some(k) => k,
                None => return Err(CoercionError::type_error(STRING_TYPE, key).into()),
            };

            let key = MetadataKeyRef::new(key)?;

            let value = value.to_json_value().with_context(|| {
                MetadataAttrTypeCoerceError::ValueIsNotJson {
                    key: key.to_owned(),
                    value: value.to_repr(),
                }
            })?;

            map.insert(key.to_owned(), value);
        }

        Ok(CoercedAttr::Metadata(MetadataMap::new(map)))
    }

    fn starlark_type(&self) -> TyMaybeSelect {
        TyMaybeSelect::Basic(OpaqueMetadata::starlark_type_repr())
    }
}
