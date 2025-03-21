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

use std::cell::RefCell;
use std::fmt::Debug;

use thiserror::Error;

use crate::codemap::Span;
use crate::codemap::Spanned;
use crate::eval::compiler::scope::payload::CstAssign;
use crate::eval::compiler::scope::payload::CstExpr;
use crate::eval::compiler::scope::payload::CstPayload;
use crate::eval::compiler::scope::BindingId;
use crate::eval::compiler::scope::ResolvedIdent;
use crate::slice_vec_ext::SliceExt;
use crate::slice_vec_ext::VecExt;
use crate::syntax::ast::ArgumentP;
use crate::syntax::ast::AssignOp;
use crate::syntax::ast::AssignP;
use crate::syntax::ast::AstLiteral;
use crate::syntax::ast::BinOp;
use crate::syntax::ast::ClauseP;
use crate::syntax::ast::ExprP;
use crate::syntax::ast::ForClauseP;
use crate::typing::basic::TyBasic;
use crate::typing::bindings::BindExpr;
use crate::typing::error::TypingError;
use crate::typing::function::Arg;
use crate::typing::oracle::ctx::TypingOracleCtx;
use crate::typing::oracle::traits::TypingAttr;
use crate::typing::oracle::traits::TypingBinOp;
use crate::typing::oracle::traits::TypingUnOp;
use crate::typing::ty::Approximation;
use crate::typing::ty::Ty;
use crate::typing::unordered_map::UnorderedMap;
use crate::typing::OracleDocs;
use crate::typing::TypingOracle;

#[derive(Error, Debug)]
enum TypingContextError {
    #[error("The attribute `{attr}` is not available on the type `{typ}`")]
    AttributeNotAvailable { typ: String, attr: String },
    #[error("The builtin `{name}` is not known")]
    UnknownBuiltin { name: String },
    #[error("Unary operator `{un_op}` is not available on the type `{ty}`")]
    UnaryOperatorNotAvailable { un_op: TypingUnOp, ty: Ty },
    #[error("Binary operator `{bin_op}` is not available on the types `{left}` and `{right}`")]
    BinaryOperatorNotAvailable {
        bin_op: TypingBinOp,
        left: Ty,
        right: Ty,
    },
}

pub(crate) struct TypingContext<'a> {
    pub(crate) oracle: TypingOracleCtx<'a>,
    pub(crate) global_docs: OracleDocs,
    // We'd prefer this to be a &mut self,
    // but that makes writing the code more fiddly, so just RefCell the errors
    pub(crate) errors: RefCell<Vec<TypingError>>,
    pub(crate) approximoations: RefCell<Vec<Approximation>>,
    pub(crate) types: UnorderedMap<BindingId, Ty>,
}

impl TypingContext<'_> {
    fn add_error(&self, span: Span, err: TypingContextError) -> Ty {
        let err = self.oracle.mk_error(span, err);
        self.errors.borrow_mut().push(err);
        Ty::never()
    }

    pub(crate) fn approximation(&self, category: &'static str, message: impl Debug) -> Ty {
        self.approximoations
            .borrow_mut()
            .push(Approximation::new(category, message));
        Ty::any()
    }

    fn validate_call(&self, fun: &Ty, args: &[Spanned<Arg>], span: Span) -> Ty {
        match self.oracle.validate_call(span, fun, args) {
            Ok(ty) => ty,
            Err(e) => {
                self.errors.borrow_mut().push(e);
                Ty::never()
            }
        }
    }

    fn from_iterated(&self, ty: &Ty, span: Span) -> Ty {
        self.expression_attribute(ty, TypingAttr::Iter, span)
    }

    pub(crate) fn validate_type(&self, got: &Ty, require: &Ty, span: Span) {
        if let Err(e) = self.oracle.validate_type(got, require, span) {
            self.errors.borrow_mut().push(e);
        }
    }

    fn builtin(&self, name: &str, span: Span) -> Ty {
        match self.global_docs.builtin(name) {
            Ok(x) => x,
            Err(()) => self.add_error(
                span,
                TypingContextError::UnknownBuiltin {
                    name: name.to_owned(),
                },
            ),
        }
    }

    fn expression_attribute(&self, ty: &Ty, attr: TypingAttr, span: Span) -> Ty {
        match self.oracle.attribute_ty(ty, attr) {
            Ok(x) => x,
            Err(()) => self.add_error(
                span,
                TypingContextError::AttributeNotAvailable {
                    typ: ty.to_string(),
                    attr: attr.to_string(),
                },
            ),
        }
    }

    fn expression_primitive_ty(
        &self,
        name: TypingAttr,
        arg0: Ty,
        args: Vec<Spanned<Ty>>,
        span: Span,
    ) -> Ty {
        let fun = self.expression_attribute(&arg0, name, span);
        self.validate_call(&fun, &args.into_map(|x| x.into_map(Arg::Pos)), span)
    }

    fn expression_primitive(&self, name: TypingAttr, args: &[&CstExpr], span: Span) -> Ty {
        let t0 = self.expression_type(args[0]);
        let ts = args[1..].map(|x| Spanned {
            span: x.span,
            node: self.expression_type(x),
        });

        // Hack for `list[str]`: list of `list` is just "function", and we don't want
        // to make it custom type and have overly complex machinery for handling it.
        // So we just special case it here.
        if t0.is_function() && name == TypingAttr::Index {
            if let ExprP::Identifier(v0) = &args[0].node {
                if v0.0 == "list" {
                    // TODO: make this "eval_type" or something.
                    return Ty::any();
                }
            }
        }

        self.expression_primitive_ty(name, t0, ts, span)
    }

    fn expression_un_op(&self, span: Span, arg: &CstExpr, un_op: TypingUnOp) -> Ty {
        let ty = self.expression_type(arg);
        if ty.is_never() || ty.is_any() {
            return ty;
        }
        let mut results = Vec::new();
        for variant in ty.iter_union() {
            match variant {
                TyBasic::StarlarkValue(ty) => match ty.un_op(un_op) {
                    Ok(x) => results.push(Ty::basic(TyBasic::StarlarkValue(x))),
                    Err(()) => {}
                },
                _ => {
                    // The rest do not support unary operators.
                }
            }
        }
        if results.is_empty() {
            self.add_error(
                span,
                TypingContextError::UnaryOperatorNotAvailable { un_op, ty },
            )
        } else {
            Ty::unions(results)
        }
    }

    pub(crate) fn expression_bind_type(&self, x: &BindExpr) -> Ty {
        match x {
            BindExpr::Expr(x) => self.expression_type(x),
            BindExpr::GetIndex(i, x) => self.expression_bind_type(x).indexed(*i),
            BindExpr::Iter(x) => self.from_iterated(&self.expression_bind_type(x), x.span()),
            BindExpr::AssignModify(lhs, op, rhs) => {
                let span = lhs.span;
                let rhs_ty = self.expression_type(rhs);
                let lhs = self.expression_assign(lhs);
                let attr = match op {
                    AssignOp::Add => TypingAttr::BinOp(TypingBinOp::Add),
                    AssignOp::Subtract => TypingAttr::BinOp(TypingBinOp::Sub),
                    AssignOp::Multiply => TypingAttr::BinOp(TypingBinOp::Mul),
                    AssignOp::Divide => TypingAttr::BinOp(TypingBinOp::Div),
                    AssignOp::FloorDivide => TypingAttr::BinOp(TypingBinOp::FloorDiv),
                    AssignOp::Percent => TypingAttr::BinOp(TypingBinOp::Percent),
                    AssignOp::BitAnd => TypingAttr::BinOp(TypingBinOp::BitAnd),
                    AssignOp::BitOr => TypingAttr::BinOp(TypingBinOp::BitOr),
                    AssignOp::BitXor => TypingAttr::BinOp(TypingBinOp::BitXor),
                    AssignOp::LeftShift => TypingAttr::BinOp(TypingBinOp::LeftShift),
                    AssignOp::RightShift => TypingAttr::BinOp(TypingBinOp::RightShift),
                };
                self.expression_primitive_ty(
                    attr,
                    lhs,
                    vec![Spanned {
                        span: rhs.span,
                        node: rhs_ty,
                    }],
                    span,
                )
            }
            BindExpr::SetIndex(id, index, e) => {
                let span = index.span;
                let index = self.expression_type(index);
                let e = self.expression_bind_type(e);
                let mut res = Vec::new();
                // We know about list and dict, everything else we just ignore
                if self.types[id].is_list() {
                    // If we know it MUST be a list, then the index must be an int
                    self.validate_type(&index, &Ty::int(), span);
                }
                for ty in self.types[id].iter_union() {
                    match ty {
                        TyBasic::List(_) => {
                            res.push(Ty::list(e.clone()));
                        }
                        TyBasic::Dict(_) => {
                            res.push(Ty::dict(index.clone(), e.clone()));
                        }
                        _ => {
                            // Either it's not something we can apply this to, in which case do nothing.
                            // Or it's an Any, in which case we aren't going to change its type or spot errors.
                        }
                    }
                }
                Ty::unions(res)
            }
            BindExpr::ListAppend(id, e) => {
                if self.oracle.probably_a_list(&self.types[id]) {
                    Ty::list(self.expression_type(e))
                } else {
                    // It doesn't seem to be a list, so let's assume the append is non-mutating
                    Ty::never()
                }
            }
            BindExpr::ListExtend(id, e) => {
                if self.oracle.probably_a_list(&self.types[id]) {
                    Ty::list(self.from_iterated(&self.expression_type(e), e.span))
                } else {
                    // It doesn't seem to be a list, so let's assume the extend is non-mutating
                    Ty::never()
                }
            }
        }
    }

    /// Used to get the type of an expression when used as part of a ModifyAssign operation
    fn expression_assign(&self, x: &CstAssign) -> Ty {
        match &**x {
            AssignP::Tuple(_) => self.approximation("expression_assignment", x),
            AssignP::Index(a_b) => {
                self.expression_primitive(TypingAttr::Index, &[&a_b.0, &a_b.1], x.span)
            }
            AssignP::Dot(_, _) => self.approximation("expression_assignment", x),
            AssignP::Identifier(x) => {
                if let Some(i) = x.1 {
                    if let Some(ty) = self.types.get(&i) {
                        return ty.clone();
                    }
                }
                panic!("Unknown identifier")
            }
        }
    }

    /// We don't need the type out of the clauses (it doesn't change the overall type),
    /// but it is important we see through to the nested expressions to raise errors
    fn check_comprehension(&self, for_: &ForClauseP<CstPayload>, clauses: &[ClauseP<CstPayload>]) {
        self.expression_type(&for_.over);
        for x in clauses {
            match x {
                ClauseP::For(x) => self.expression_type(&x.over),
                ClauseP::If(x) => self.expression_type(x),
            };
        }
    }

    pub(crate) fn expression_type_spanned(&self, x: &CstExpr) -> Spanned<Ty> {
        Spanned {
            span: x.span,
            node: self.expression_type(x),
        }
    }

    fn expr_bin_op_ty_basic(
        &self,
        span: Span,
        lhs: Spanned<&TyBasic>,
        bin_op: TypingBinOp,
        rhs: Spanned<&TyBasic>,
    ) -> Result<Ty, ()> {
        if let TyBasic::StarlarkValue(lhs) = &lhs.node {
            return lhs.bin_op(bin_op, rhs.node);
        }

        let fun = match self.oracle.attribute(&lhs.node, TypingAttr::BinOp(bin_op)) {
            Some(Ok(fun)) => fun,
            Some(Err(())) => return Err(()),
            None => return Ok(Ty::any()),
        };
        self.oracle
            .validate_call(
                span,
                &fun,
                &[rhs.into_map(|t| Arg::Pos(Ty::basic(t.clone())))],
            )
            .map_err(|_| ())
    }

    fn expr_bin_op_ty(
        &self,
        span: Span,
        lhs: Spanned<Ty>,
        bin_op: TypingBinOp,
        rhs: Spanned<Ty>,
    ) -> Ty {
        if lhs.is_never() || rhs.is_never() {
            return Ty::never();
        }

        let mut good = Vec::new();
        for lhs_i in lhs.node.iter_union() {
            for rhs_i in rhs.node.iter_union() {
                let lhs_i = Spanned {
                    span: lhs.span,
                    node: lhs_i,
                };
                let rhs_i = Spanned {
                    span: rhs.span,
                    node: rhs_i,
                };
                if let Ok(ty) = self.expr_bin_op_ty_basic(span, lhs_i, bin_op, rhs_i) {
                    good.push(ty);
                }
            }
        }

        if good.is_empty() {
            self.add_error(
                span,
                TypingContextError::BinaryOperatorNotAvailable {
                    left: lhs.node,
                    right: rhs.node,
                    bin_op,
                },
            );
            Ty::never()
        } else {
            Ty::unions(good)
        }
    }

    fn expr_bin_op(&self, span: Span, lhs: &CstExpr, op: BinOp, rhs: &CstExpr) -> Ty {
        let lhs = self.expression_type_spanned(lhs);
        let rhs = self.expression_type_spanned(rhs);
        let bool_ret = if lhs.is_never() || rhs.is_never() {
            Ty::never()
        } else {
            Ty::bool()
        };
        match op {
            BinOp::And | BinOp::Or => {
                if lhs.is_never() {
                    Ty::never()
                } else {
                    Ty::union2(lhs.node, rhs.node)
                }
            }
            BinOp::Equal | BinOp::NotEqual => {
                // It's not an error to compare two different types, but it is pointless
                self.validate_type(&lhs, &rhs, span);
                bool_ret
            }
            BinOp::In | BinOp::NotIn => {
                // We dispatch `x in y` as y.__in__(x) as that's how we validate
                self.expr_bin_op_ty(span, rhs, TypingBinOp::In, lhs);
                // Ignore the return type, we know it's always a bool
                bool_ret
            }
            BinOp::Less | BinOp::LessOrEqual | BinOp::Greater | BinOp::GreaterOrEqual => {
                self.expr_bin_op_ty(span, lhs, TypingBinOp::Less, rhs);
                bool_ret
            }
            BinOp::Subtract => self.expr_bin_op_ty(span, lhs, TypingBinOp::Sub, rhs),
            BinOp::Add => self.expr_bin_op_ty(span, lhs, TypingBinOp::Add, rhs),
            BinOp::Multiply => self.expr_bin_op_ty(span, lhs, TypingBinOp::Mul, rhs),
            BinOp::Percent => self.expr_bin_op_ty(span, lhs, TypingBinOp::Percent, rhs),
            BinOp::Divide => self.expr_bin_op_ty(span, lhs, TypingBinOp::Div, rhs),
            BinOp::FloorDivide => self.expr_bin_op_ty(span, lhs, TypingBinOp::FloorDiv, rhs),
            BinOp::BitAnd => self.expr_bin_op_ty(span, lhs, TypingBinOp::BitAnd, rhs),
            BinOp::BitOr => self.expr_bin_op_ty(span, lhs, TypingBinOp::BitOr, rhs),
            BinOp::BitXor => self.expr_bin_op_ty(span, lhs, TypingBinOp::BitXor, rhs),
            BinOp::LeftShift => self.expr_bin_op_ty(span, lhs, TypingBinOp::LeftShift, rhs),
            BinOp::RightShift => self.expr_bin_op_ty(span, lhs, TypingBinOp::RightShift, rhs),
        }
    }

    pub(crate) fn expression_type(&self, x: &CstExpr) -> Ty {
        let span = x.span;
        match &**x {
            ExprP::Tuple(xs) => Ty::tuple(xs.map(|x| self.expression_type(x))),
            ExprP::Dot(a, b) => {
                self.expression_attribute(&self.expression_type(a), TypingAttr::Regular(b), b.span)
            }
            ExprP::Call(f, args) => {
                let args_ty: Vec<Spanned<Arg>> = args.map(|x| Spanned {
                    span: x.span,
                    node: match &**x {
                        ArgumentP::Positional(x) => Arg::Pos(self.expression_type(x)),
                        ArgumentP::Named(name, x) => {
                            Arg::Name((**name).clone(), self.expression_type(x))
                        }
                        ArgumentP::Args(x) => {
                            let ty = self.expression_type(x);
                            self.from_iterated(&ty, x.span);
                            Arg::Args(ty)
                        }
                        ArgumentP::KwArgs(x) => {
                            let ty = self.expression_type(x);
                            self.validate_type(&ty, &Ty::dict(Ty::string(), Ty::any()), x.span);
                            Arg::Kwargs(ty)
                        }
                    },
                });
                let f_ty = self.expression_type(f);
                // If we can't resolve the types of the arguments, we can't validate the call,
                // but we still know the type of the result since the args don't impact that
                self.validate_call(&f_ty, &args_ty, span)
            }
            ExprP::Index(a_b) => {
                self.expression_primitive(TypingAttr::Index, &[&a_b.0, &a_b.1], span)
            }
            ExprP::Index2(a_i0_i1) => {
                let (a, i0, i1) = &**a_i0_i1;
                self.expression_type(a);
                self.expression_type(i0);
                self.expression_type(i1);
                Ty::any()
            }
            ExprP::Slice(x, start, stop, stride) => {
                for e in [start, stop, stride].iter().copied().flatten() {
                    self.validate_type(&self.expression_type(e), &Ty::int(), e.span);
                }
                self.expression_attribute(&self.expression_type(x), TypingAttr::Slice, span)
            }
            ExprP::Identifier(x) => {
                match &x.node.1 {
                    Some(ResolvedIdent::Slot(_, i)) => {
                        if let Some(ty) = self.types.get(i) {
                            ty.clone()
                        } else {
                            // All types must be resolved to this point,
                            // this code is unreachable.
                            Ty::any()
                        }
                    }
                    Some(ResolvedIdent::Global(g)) => {
                        if let Some(t) = g.to_value().get_ref().typechecker_ty() {
                            t
                        } else {
                            self.builtin(&x.node.0, x.span)
                        }
                    }
                    None => {
                        // All identifiers must be resolved at this point,
                        // but we don't stop after scope resolution error,
                        // so this code is reachable.
                        Ty::any()
                    }
                }
            }
            ExprP::Lambda(_) => {
                self.approximation("We don't type check lambdas", ());
                Ty::any_function()
            }
            ExprP::Literal(x) => match x {
                AstLiteral::Int(_) => Ty::int(),
                AstLiteral::Float(_) => Ty::float(),
                AstLiteral::String(_) => Ty::string(),
            },
            ExprP::Not(x) => {
                if self.expression_type(x).is_never() {
                    Ty::never()
                } else {
                    Ty::bool()
                }
            }
            ExprP::Minus(x) => self.expression_un_op(span, x, TypingUnOp::Minus),
            ExprP::Plus(x) => self.expression_un_op(span, x, TypingUnOp::Plus),
            ExprP::BitNot(x) => self.expression_un_op(span, x, TypingUnOp::BitNot),
            ExprP::Op(lhs, op, rhs) => self.expr_bin_op(span, lhs, *op, rhs),
            ExprP::If(c_t_f) => {
                let c = self.expression_type(&c_t_f.0);
                let t = self.expression_type(&c_t_f.1);
                let f = self.expression_type(&c_t_f.2);
                if c.is_never() {
                    Ty::never()
                } else {
                    Ty::union2(t, f)
                }
            }
            ExprP::List(xs) => {
                let ts = xs.map(|x| self.expression_type(x));
                Ty::list(Ty::unions(ts))
            }
            ExprP::Dict(xs) => {
                let (ks, vs) = xs
                    .iter()
                    .map(|(k, v)| (self.expression_type(k), self.expression_type(v)))
                    .unzip();
                Ty::dict(Ty::unions(ks), Ty::unions(vs))
            }
            ExprP::ListComprehension(a, b, c) => {
                self.check_comprehension(b, c);
                Ty::list(self.expression_type(a))
            }
            ExprP::DictComprehension(k_v, b, c) => {
                self.check_comprehension(b, c);
                Ty::dict(self.expression_type(&k_v.0), self.expression_type(&k_v.1))
            }
            ExprP::FString(_) => Ty::string(),
        }
    }
}
