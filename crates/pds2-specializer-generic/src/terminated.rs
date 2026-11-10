use anyhow::{anyhow, ensure, Context as _, Result};
use genco::lang::{rust, Lang, Rust};
use genco::prelude::*;
use genco::Tokens;

use pds2_core::ast::Ident;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir;
use pds2_core::specializer::encode::{Args, Encoder, Encoding, Val};
use pds2_core::specializer::expr::{Elaborator, ValueModel};
use pds2_core::specializer::reflect::{list_of, use_of};
use pds2_core::specializer::{
	construct_vec, list_value, pds_io, vec_of_value, visit_specialization, AstHir, Embed, LowerCx, Names,
	NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor, VALUE,
};

impl ValueModel for Terminated {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &mir::Expr,
	) -> Result<mir::Construction> {
		list_value(cx, id, native, value)
	}
}

pub struct Terminated;

impl NativeLayout for Terminated {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}

pub fn terminator<L: Lang>(ctx: &NativeCodegenCtx<L>) -> Result<(TypeId, Tokens<L>)> {
	let end = ctx
		.native
		.ref_args
		.iter()
		.find(|(n, _)| n.as_str() == "end")
		.map(|(_, e)| e)
		.ok_or_else(|| anyhow!("terminated array needs `end`, declared as `argument(end) => <type>`"))?;
	let ty = end
		.ty()
		.ok_or_else(|| anyhow!("terminator `end` has no type, declare `argument(end) => <type>`"))?;
	Ok((ty, (ctx.lower)(end, false)?))
}

impl NativeCodegen<Rust> for Terminated {
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		vec_of_value(native, names)
	}
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		construct_vec(cx, c)
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("terminated array value")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("terminated array type")?;
		let (end_ty, end) = terminator(ctx)?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				for e in _0.iter() {
					$(ctx.names.serialize(value))(e, output)?;
				}
				$(ctx.names.serialize(end_ty))(&$(&end), output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let mut out = Vec::new();
				loop {
					let at = input.offset();
					if $(ctx.names.deserialize(end_ty))(input)? == $(&end) {
						return Ok(out);
					}
					input.rewind(at);
					out.push($(ctx.deserialize_call(value_use))?);
				}
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = $(ctx.names.size_of(end_ty))(&$(&end));
				for e in _0.iter() { n += $(ctx.names.size_of(value))(e); }
				n
			}
		})
	}
}

impl AstHir for Terminated {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ensure!(
			ctx.body.is_empty(),
			"terminated array does not accept a body, got {:?}",
			ctx.body
		);
		ensure!(
			ctx.args.iter().any(|(n, _)| n.as_str() == "end"),
			"terminated array needs an `end:` argument"
		);
		Ok(vec![])
	}
}

impl Encoder for Terminated {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let end = v
			.native
			.ref_args
			.iter()
			.find(|(n, _)| n.as_str() == "end")
			.map(|(_, e)| e)
			.ok_or_else(|| anyhow!("terminated array without `end`"))?;
		let end = cx.eval(end, &v.scope())?;
		let mut terminator = Vec::new();
		cx.encode(&end, &Args::new(), &mut terminator).context("end")?;
		let value = use_of(v.native, VALUE)?;
		for (i, item) in list_of(v.c)?.iter().enumerate() {
			let mut buf = Vec::new();
			cx.encode_use(value, item, &v.scope(), &mut buf)
				.with_context(|| format!("item {i}"))?;
			ensure!(
				!buf.starts_with(&terminator),
				"item {i} starts with the terminator {terminator:?}, reading would stop there"
			);
			out.extend(buf);
		}
		out.extend(terminator);
		Ok(())
	}
}
