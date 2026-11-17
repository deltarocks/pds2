use anyhow::{anyhow, ensure, Context as _, Result};
use genco::lang::{rust, Lang, Rust};
use genco::prelude::*;
use genco::Tokens;

use pds2_core::ast::Ident;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir;
use pds2_core::specializer::eval::{const_eq, Args, Target, Val};
use pds2_specializer_bytes::{Encoder, Encoding, Input};
use pds2_core::specializer::expr::{Elaborator, ValueModel};
use pds2_core::specializer::reflect::{list_of, use_of};
use pds2_core::specializer::{
	construct_vec, list_value, pds_io, vec_of_value, visit_specialization, AstHir, Embed, LowerCx,
	Names, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor, ViewPolicy, into_view_vec,
	view_vec, VALUE,
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
	fn view_policy(&self) -> ViewPolicy {
		ViewPolicy::Composite
	}
	fn view_take(&self) -> bool {
		true
	}
	fn view_heap(&self) -> bool {
		true
	}
	fn into_view(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<Option<rust::Tokens>> {
		let value = ctx.specialization("Value").context("terminated array value")?;
		Ok(Some(into_view_vec(ctx, value)))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("terminated array value")?;
		let value = value_use.target;
		let ty = vec_of_value(ctx.native, ctx.names).context("terminated array type")?;
		let (end_ty, end) = terminator(ctx)?;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let lt = &ctx.lt();
		let this = &ctx.this();
		Ok(quote! {
			$((ctx.args_struct)())
			pub type $this = $ty;
			pub fn serialize$lt(_0: &$this, output: &mut $poutput) -> $presult<()> {
				for e in _0.iter() {
					$(ctx.names.serialize(value))(e, output)?;
				}
				$(ctx.names.serialize(end_ty))(&$(&end), output)
			}
			pub fn deserialize$lt(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<$this> {
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
			pub fn size_of$lt(_0: &$this) -> usize {
				let mut n = $(ctx.names.size_of(end_ty))(&$(&end));
				for e in _0.iter() { n += $(ctx.names.size_of(value))(e); }
				n
			}
			$(if ctx.view => $(view_vec(ctx, value)))
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

fn end_of(native: &mir::NativeType) -> Result<&mir::Expr> {
	native
		.ref_args
		.iter()
		.find(|(n, _)| n.as_str() == "end")
		.map(|(_, e)| e)
		.ok_or_else(|| anyhow!("terminated array without `end`"))
}

impl Encoder for Terminated {
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		pds2_core::specializer::specialization_use(native, "End").map_or(0, |e| cx.min_size(e.target))
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let end = cx.eval(end_of(v.native)?, &v.scope())?;
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

	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<mir::Construction> {
		let end = cx.eval(end_of(t.native)?, &t.scope())?;
		let end_ty = end
			.ty()
			.ok_or_else(|| anyhow!("terminator `end` has no type"))?;
		let value = use_of(t.native, VALUE)?;
		let mut items = Vec::new();
		loop {
			let at = input.offset();
			let next = cx.decode(end_ty, &Args::new(), input).context("end")?;
			if const_eq(&next, &end) {
				return Ok(mir::Construction::List(items));
			}
			input.rewind(at);
			let item = cx
				.decode_use(value, &t.scope(), input)
				.with_context(|| format!("item {}", items.len()))?;
			items.push(item);
		}
	}
}
