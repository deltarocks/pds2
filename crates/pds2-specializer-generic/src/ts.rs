use anyhow::{Context as _, Result};
use genco::lang::JavaScript;
use genco::prelude::*;

use pds2_core::hir::TypeId;
use pds2_core::mir;
use pds2_core::specializer::ts::{array_ty, construct_array, from_list, pds, value_of, Ctx, Tokens};
use pds2_core::specializer::{LowerCx, Names, NativeCodegen, Scalar, VALUE};

use crate::terminated::{terminator, Terminated};

impl NativeCodegen<JavaScript> for Terminated {
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<JavaScript>,
	) -> Option<Tokens> {
		Some(array_ty(names, value_of(native)?))
	}
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		construct_array(cx, native, c)
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let value_use = ctx
			.specialization_use(VALUE)
			.context("terminated array value")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("terminated array type")?;
		let (end_ty, end) = terminator(ctx)?;
		let is_end = if ctx.names.scalar(end_ty).is_some_and(|s| s != Scalar::Bytes) {
			quote!(($(ctx.names.deserialize(end_ty))(input) === $(&end)))
		} else {
			quote!($(pds("equal"))($(ctx.names.deserialize(end_ty))(input), $(&end)))
		};
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = $ty;
			export function serialize(_0: Ty, output: $poutput): void {
				for (const e of _0) {
					$(ctx.names.serialize(value))(e, output);
				}
				$(ctx.names.serialize(end_ty))($(&end), output);
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				const out: Array<$(ctx.names.ty(value))> = [];
				for (;;) {
					const at = input.offset;
					if ($is_end) {
						break;
					}
					input.rewind(at);
					out.push($(ctx.deserialize_call(value_use)));
				}
				return $(from_list(ctx, value));
			}
			export function size_of(_0: Ty): number {
				let n = $(ctx.names.size_of(end_ty))($(&end));
				for (const e of _0) {
					n += $(ctx.names.size_of(value))(e);
				}
				return n;
			}
		})
	}
}
