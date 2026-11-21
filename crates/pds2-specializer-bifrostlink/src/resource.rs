use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::{rust, Rust};
use genco::prelude::*;
use pds2_core::ast::Ident;
use pds2_core::hir::TypeId;
use pds2_core::mir::{self, Construction};
use pds2_core::specializer::eval::{Target, Val};
use pds2_core::specializer::{
	pds_io, specialization_use, visit_specialization, BoxCost, Embed, Names, NativeCodegen,
	NativeCodegenCtx, NativeLayout, SlotVisitor, VALUE,
};
use pds2_specializer_bytes::{Encoder, Encoding, Input};

pub struct ResourceNative;

impl NativeLayout for ResourceNative {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Boxable(BoxCost::Low), v);
	}
}

impl NativeCodegen<Rust> for ResourceNative {
	fn inline_ty(
		&self,
		id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		let value = specialization_use(native, VALUE)?.target;
		Some(quote!(::bifrostlink::Resource<$(names.slot_ty(id, &Ident::new(VALUE), value))>))
	}
	fn value_traits(&self, _native: &mir::NativeType) -> bool {
		false
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use(VALUE)
			.context("resource payload type")?;
		let value = value_use.target;
		let slot = Ident::new(VALUE);
		let payload = ctx.slot_ty(&slot, value);
		let read = ctx.slot_new(&slot, quote!($(ctx.deserialize_call(value_use))?));
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = ::bifrostlink::Resource<$payload>;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				::bifrostlink::__private::resource::serialize(_0, output)?;
				$(ctx.names.serialize(value))(&**_0, output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let id = ::bifrostlink::__private::resource::read_id(input)?;
				let payload = $read;
				::bifrostlink::__private::resource::adopt(id, payload)
			}
			pub fn size_of(_0: &Ty) -> usize {
				::bifrostlink::__private::resource::id_size(_0) + $(ctx.names.size_of(value))(&**_0)
			}
		})
	}
}

impl Encoder for ResourceNative {
	fn encode(&self, _cx: &Encoding, _v: &Val, _out: &mut Vec<u8>) -> Result<()> {
		bail!("resources exist only at runtime")
	}
	fn decode(&self, _cx: &Encoding, _t: &Target, _input: &mut Input) -> Result<Construction> {
		Err(anyhow!("resources exist only at runtime"))
	}
}
