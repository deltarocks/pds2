use anyhow::{anyhow, bail, ensure, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use pds2_core::specializer::encode::{Encoder, Encoding, Val};
use pds2_core::specializer::expr::{Elaborator, ValueModel};
use pds2_core::specializer::{
	AstHir, Embed, LowerCx, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor, VALUE,
};
use pds2_core::ast::Ident;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr};

pub struct Const;

fn value_of<'c, L: Lang>(ctx: &NativeCodegenCtx<'c, L>) -> Result<(&'c mir::Type, Tokens<L>)> {
	let value_use = pds2_core::specializer::specialization_use(ctx.native, VALUE)
		.ok_or_else(|| anyhow!("const needs its `Value` type"))?;
	let value = ctx
		.native
		.ref_args
		.iter()
		.find(|(n, _)| n.as_str() == "value")
		.map(|(_, e)| e)
		.ok_or_else(|| anyhow!("const needs `value`, declared as `argument(value) => Value; body;`"))?;
	Ok((value_use, (ctx.lower)(value, false)?))
}

fn inner(c: &Construction) -> Result<&Expr> {
	match c {
		Construction::Wrap(e) => Ok(e),
		c => bail!("const is constructed from its value, got {c:?}"),
	}
}

impl ValueModel for Const {
	fn construct(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let ty = pds2_core::specializer::specialization_use(native, VALUE)
			.ok_or_else(|| anyhow!("const needs its `Value` type"))?
			.target;
		Ok(Construction::Wrap(Box::new(cx.coerce(value.clone(), ty)?)))
	}
}

impl Encoder for Const {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let value_use = pds2_core::specializer::specialization_use(v.native, VALUE)
			.ok_or_else(|| anyhow!("const needs its `Value` type"))?;
		cx.encode_use(value_use, inner(v.c)?, &v.scope(), out)
	}
}

impl AstHir for Const {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ensure!(ctx.body.is_empty(), "const does not accept a body");
		ensure!(
			ctx.args.iter().any(|(n, _)| n.as_str() == "value"),
			"const needs a `value:` argument"
		);
		Ok(vec![])
	}
}

impl NativeLayout for Const {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		pds2_core::specializer::visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}

impl NativeCodegen<Rust> for Const {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let (value_use, built) = value_of(ctx)?;
		let ty = value_use.target;
		let names = ctx.names;
		let (pinput, poutput, presult) = pds2_core::specializer::pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $(names.ty(ty));
			pub fn value() -> Ty {
				$built
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(names.serialize(ty))(_0, output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				$(ctx.deserialize_call(value_use))
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(names.size_of(ty))(_0)
			}
		})
	}
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		(cx.lower)(inner(c)?)
	}
}

impl NativeCodegen<JavaScript> for Const {
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let (value_use, built) = value_of(ctx)?;
		let ty = value_use.target;
		let names = ctx.names;
		let pinput = &js::import("../pds.ts", "Input");
		let poutput = &js::import("../pds.ts", "Output");
		let pass = (ctx.pass_args)(value_use);
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = $(names.ty(ty));
			export function value(): Ty {
				return $built;
			}
			export function serialize(_0: Ty, output: $poutput): void {
				$(names.serialize(ty))(_0, output);
			}
			export function deserialize(input: $pinput): Ty {
				return $(names.deserialize(ty))(input$(if !pass.is_empty() => , $pass));
			}
			export function size_of(_0: Ty): number {
				return $(names.size_of(ty))(_0);
			}
		})
	}
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<js::Tokens> {
		(cx.lower)(inner(c)?)
	}
}
