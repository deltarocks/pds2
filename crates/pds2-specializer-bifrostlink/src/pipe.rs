use anyhow::{Result, anyhow, bail};
use genco::lang::{Rust, rust};
use genco::prelude::*;
use pds2_core::hir::TypeId;
use pds2_core::mir::{self, Construction};
use pds2_core::specializer::eval::{Target, Val};
use pds2_core::specializer::{
	Names, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor, pds_io,
};
use pds2_specializer_bytes::{Encoder, Encoding, Input};

pub struct PipeNative;

impl NativeLayout for PipeNative {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}

impl NativeCodegen<Rust> for PipeNative {
	fn inline_ty(
		&self,
		_id: TypeId,
		_native: &mir::NativeType,
		_names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		Some(quote!(::bifrostlink::Pipe))
	}
	fn value_traits(&self, _native: &mir::NativeType) -> bool {
		false
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = ::bifrostlink::Pipe;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				::bifrostlink::__private::pipe::serialize(_0, output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				::bifrostlink::__private::pipe::deserialize(input)
			}
			pub fn size_of(_0: &Ty) -> usize {
				::bifrostlink::__private::pipe::size_of(_0)
			}
		})
	}
}

impl Encoder for PipeNative {
	fn encode(&self, _cx: &Encoding, _v: &Val, _out: &mut Vec<u8>) -> Result<()> {
		bail!("pipes exist only at runtime")
	}
	fn decode(&self, _cx: &Encoding, _t: &Target, _input: &mut Input) -> Result<Construction> {
		Err(anyhow!("pipes exist only at runtime"))
	}
}
