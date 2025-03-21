/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::cmp::Ordering;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;
use std::hash::Hash;
use std::hash::Hasher;
use std::marker::PhantomData;

use allocative::Allocative;
use dupe::Dupe;

use crate::docs::DocItem;
use crate::typing::Ty;
use crate::typing::TyBasic;
use crate::typing::TypingBinOp;
use crate::typing::TypingUnOp;
use crate::values::traits::StarlarkValueVTable;
use crate::values::traits::StarlarkValueVTableGet;
use crate::values::StarlarkValue;

// This is a bit suboptimal for binary size:
// we have two vtable instances for each type: this one, and the one within `AValue` vtable.
struct TyStarlarkValueVTable {
    type_name: &'static str,
    // TODO(nga): put these into generated `StarlarkValueVTable`.
    has_plus: bool,
    has_minus: bool,
    has_bit_not: bool,
    vtable: StarlarkValueVTable,
}

struct TyStarlarkValueVTableGet<'v, T: StarlarkValue<'v>>(PhantomData<&'v T>);

impl<'v, T: StarlarkValue<'v>> TyStarlarkValueVTableGet<'v, T> {
    const VTABLE: TyStarlarkValueVTable = TyStarlarkValueVTable {
        type_name: T::TYPE,
        has_plus: T::HAS_PLUS,
        has_minus: T::HAS_MINUS,
        has_bit_not: T::HAS_BIT_NOT,
        vtable: StarlarkValueVTableGet::<T>::VTABLE,
    };
}

/// Type implementation where typing is handled by the `StarlarkValue` trait implementation.
#[derive(Allocative, Clone, Copy, Dupe)]
pub struct TyStarlarkValue {
    #[allocative(skip)]
    vtable: &'static TyStarlarkValueVTable,
}

impl Debug for TyStarlarkValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TyStarlarkValue")
            .field("type_name", &self.vtable.type_name)
            .finish_non_exhaustive()
    }
}

impl Display for TyStarlarkValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.vtable.type_name {
            "string" => write!(f, "str"),
            "NoneType" => write!(f, "None"),
            type_name => write!(f, "{}", type_name),
        }
    }
}

impl PartialEq for TyStarlarkValue {
    fn eq(&self, other: &Self) -> bool {
        // TODO(nga): compare type ids instead, here and below.
        self.vtable.type_name == other.vtable.type_name
    }
}

impl Eq for TyStarlarkValue {}

impl Hash for TyStarlarkValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.vtable.type_name.hash(state);
    }
}

impl PartialOrd for TyStarlarkValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TyStarlarkValue {
    fn cmp(&self, other: &Self) -> Ordering {
        self.vtable.type_name.cmp(other.vtable.type_name)
    }
}

impl TyStarlarkValue {
    pub(crate) const fn new<'v, T: StarlarkValue<'v>>() -> TyStarlarkValue {
        TyStarlarkValue {
            vtable: &TyStarlarkValueVTableGet::<T>::VTABLE,
        }
    }

    pub(crate) fn as_name(self) -> &'static str {
        self.vtable.type_name
    }

    /// Result of applying unary operator to this type.
    pub(crate) fn un_op(self, un_op: TypingUnOp) -> Result<TyStarlarkValue, ()> {
        let has = match un_op {
            TypingUnOp::Plus => self.vtable.has_plus,
            TypingUnOp::Minus => self.vtable.has_minus,
            TypingUnOp::BitNot => self.vtable.has_bit_not,
        };
        if has { Ok(self) } else { Err(()) }
    }

    pub(crate) fn bin_op(self, op: TypingBinOp, rhs: &TyBasic) -> Result<Ty, ()> {
        // TODO(nga): use `rbin_op_ty` too.
        let _todo_use_rbin_op_ty_too = self.vtable.vtable.rbin_op_ty;
        match (self.vtable.vtable.bin_op_ty)(op, rhs) {
            Some(ty) => Ok(ty),
            None => Err(()),
        }
    }

    pub(crate) fn attr(self, name: &str) -> Result<Ty, ()> {
        if let Some(methods) = (self.vtable.vtable.get_methods)() {
            if let Some(method) = methods.get(name) {
                if let Some(doc) = method.documentation() {
                    match doc {
                        DocItem::Function(method) => return Ok(Ty::from_docs_function(&method)),
                        DocItem::Property(property) => {
                            return Ok(Ty::from_docs_property(&property));
                        }
                        DocItem::Object(_) | DocItem::Module(_) => {
                            // unreachable: only methods and properties are in `methods`.
                        }
                    }
                } else {
                    // unreachable: both methods and attributes have documentation.
                }
            }
        }
        if let Some(ty) = (self.vtable.vtable.attr_ty)(name) {
            return Ok(ty);
        }
        Err(())
    }
}
