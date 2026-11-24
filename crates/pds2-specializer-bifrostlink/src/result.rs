use anyhow::{Context as _, Result, anyhow, bail};
use genco::lang::{JavaScript, Rust, js, rust};
use genco::prelude::*;
use pds2_core::ast::Ident;
use pds2_core::hir::TypeId;
use pds2_core::mir::{self, Construction};
use pds2_core::specializer::eval::{Target, Val};
use pds2_core::specializer::{
	BoxCost, Embed, Names, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor, pds_io,
	specialization_use, visit_specialization,
};
use pds2_specializer_bytes::{Encoder, Encoding, Input};

pub struct ResultNative;

const OK: &str = "Ok";
const ERR: &str = "Err";

impl NativeLayout for ResultNative {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, OK, Embed::Boxable(BoxCost::Low), v);
		visit_specialization(native, ERR, Embed::Boxable(BoxCost::Low), v);
	}
}

impl NativeCodegen<Rust> for ResultNative {
	fn inline_ty(
		&self,
		id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		let ok = specialization_use(native, OK)?.target;
		let err = specialization_use(native, ERR)?.target;
		Some(quote!(::core::result::Result<
			$(names.slot_ty(id, &Ident::new(OK), ok)),
			$(names.slot_ty(id, &Ident::new(ERR), err))
		>))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let ok_use = ctx.specialization_use(OK).context("result ok type")?;
		let err_use = ctx.specialization_use(ERR).context("result err type")?;
		let (ok, err) = (ok_use.target, err_use.target);
		let (ok_slot, err_slot) = (Ident::new(OK), Ident::new(ERR));
		let tag = (ctx.resolve)("u8").ok_or_else(|| anyhow!("result needs the `u8` native"))?;
		let read_ok = ctx.slot_new(&ok_slot, quote!($(ctx.deserialize_call(ok_use))?));
		let read_err = ctx.slot_new(&err_slot, quote!($(ctx.deserialize_call(err_use))?));
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let names = ctx.names;
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = ::core::result::Result<$(ctx.slot_ty(&ok_slot, ok)), $(ctx.slot_ty(&err_slot, err))>;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				match _0 {
					::core::result::Result::Ok(v) => {
						$(names.serialize(tag))(&0, output)?;
						$(names.serialize(ok))(v, output)
					}
					::core::result::Result::Err(e) => {
						$(names.serialize(tag))(&1, output)?;
						$(names.serialize(err))(e, output)
					}
				}
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				match $(names.deserialize(tag))(input)? {
					0 => ::core::result::Result::Ok(::core::result::Result::Ok($read_ok)),
					1 => ::core::result::Result::Ok(::core::result::Result::Err($read_err)),
					_ => ::core::result::Result::Err($(ctx.runtime("Error"))::InvalidValue),
				}
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(names.size_of(tag))(&0)
					+ match _0 {
						::core::result::Result::Ok(v) => $(names.size_of(ok))(v),
						::core::result::Result::Err(e) => $(names.size_of(err))(e),
					}
			}
		})
	}
}

impl NativeCodegen<JavaScript> for ResultNative {
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<JavaScript>,
	) -> Option<js::Tokens> {
		let ok = specialization_use(native, OK)?.target;
		let err = specialization_use(native, ERR)?.target;
		Some(
			quote!({ kind: "ok"; value: $(names.ty(ok)) } | { kind: "err"; value: $(names.ty(err)) }),
		)
	}
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let ok_use = ctx.specialization_use(OK).context("result ok type")?;
		let err_use = ctx.specialization_use(ERR).context("result err type")?;
		let (ok, err) = (ok_use.target, err_use.target);
		let tag = (ctx.resolve)("u8").ok_or_else(|| anyhow!("result needs the `u8` native"))?;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("result type")?;
		let (pinput, poutput) = (&ctx.runtime("Input"), &ctx.runtime("Output"));
		let perror = &ctx.runtime("PdsError");
		let names = ctx.names;
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = $ty;
			export function serialize(_0: Ty, output: $poutput): void {
				if (_0.kind === "ok") {
					$(names.serialize(tag))(0, output);
					$(names.serialize(ok))(_0.value, output);
				} else {
					$(names.serialize(tag))(1, output);
					$(names.serialize(err))(_0.value, output);
				}
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				switch ($(names.deserialize(tag))(input)) {
					case 0: return { kind: "ok", value: $(ctx.deserialize_call(ok_use)) };
					case 1: return { kind: "err", value: $(ctx.deserialize_call(err_use)) };
					default: throw new $perror("InvalidValue", "result tag");
				}
			}
			export function size_of(_0: Ty): number {
				return $(names.size_of(tag))(0) + (_0.kind === "ok" ? $(names.size_of(ok))(_0.value) : $(names.size_of(err))(_0.value));
			}
		})
	}
}

impl Encoder for ResultNative {
	fn encode(&self, _cx: &Encoding, _v: &Val, _out: &mut Vec<u8>) -> Result<()> {
		bail!("result values exist only at runtime")
	}
	fn decode(&self, _cx: &Encoding, _t: &Target, _input: &mut Input) -> Result<Construction> {
		Err(anyhow!("result values exist only at runtime"))
	}
}
