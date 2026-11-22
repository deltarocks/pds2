use anyhow::{Context as _, Result, anyhow, bail};
use genco::lang::JavaScript;
use genco::prelude::*;

use pds2_core::hir::TypeId;
use pds2_core::mir;
use pds2_core::specializer::ts::{
	Ctx, Tokens, array_ty, construct_array, from_list, pds, typed_array, value_of,
};
use pds2_core::specializer::{LowerCx, Names, NativeCodegen, VALUE};

use crate::natives::{RegistryEntryHolder, RegistryEntryHolderSet, TopBitSetTerminatedArray};

impl NativeCodegen<JavaScript> for RegistryEntryHolder {
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		let (kind, value) = crate::natives::holder_payload_js(cx, c, None)?;
		Ok(quote!({ kind: $(quoted(kind)), value: $value }))
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolder needs the `varint` native"))?;
		let Some((iname, ity_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolder needs an inline field from its body");
		};
		let ity = ity_use.target;
		let read_inline = ctx.slot_new(iname, ctx.deserialize_call(ity_use));
		let (pinput, poutput) = (&pds(ctx.names, "Input"), &pds(ctx.names, "Output"));
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty =
				| { kind: "id"; value: $(ctx.names.ty(varint)) }
				| { kind: "inline"; value: $(ctx.slot_ty(iname, ity)) };
			export function serialize(_0: Ty, output: $poutput): void {
				switch (_0.kind) {
					case "id":
						$(ctx.names.serialize(varint))(_0.value + 1, output);
						return;
					case "inline":
						$(ctx.names.serialize(varint))(0, output);
						$(ctx.names.serialize(ity))(_0.value, output);
						return;
				}
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				const n = $(ctx.names.deserialize(varint))(input);
				return n === 0 ? { kind: "inline", value: $read_inline } : { kind: "id", value: n - 1 };
			}
			export function size_of(_0: Ty): number {
				switch (_0.kind) {
					case "id":
						return $(ctx.names.size_of(varint))(_0.value + 1);
					case "inline":
						return $(ctx.names.size_of(varint))(0) + $(ctx.names.size_of(ity))(_0.value);
				}
			}
		})
	}
}

impl NativeCodegen<JavaScript> for RegistryEntryHolderSet {
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		let varint = cx
			.types
			.0
			.iter()
			.find(|(_, d)| d.name.readable_name() == "varint")
			.map(|(id, _)| *id);
		let typed = varint.and_then(|v| typed_array(cx.names, v));
		let (kind, value) = crate::natives::holder_payload_js(cx, c, typed)?;
		Ok(quote!({ kind: $(quoted(kind)), value: $value }))
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolderSet needs the `varint` native"))?;
		let Some((nname, nty_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolderSet needs a named field from its body");
		};
		let nty = nty_use.target;
		let read_named = ctx.slot_new(nname, ctx.deserialize_call(nty_use));
		let (pinput, poutput, perror) = (
			&pds(ctx.names, "Input"),
			&pds(ctx.names, "Output"),
			&pds(ctx.names, "PdsError"),
		);
		let ids_ty = array_ty(ctx.names, varint);
		let new_ids = match typed_array(ctx.names, varint) {
			Some(a) => quote!(new $a(count)),
			None => quote!(new Array<$(ctx.names.ty(varint))>(count)),
		};
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty =
				| { kind: "named"; value: $(ctx.slot_ty(nname, nty)) }
				| { kind: "ids"; value: $(&ids_ty) };
			export function serialize(_0: Ty, output: $poutput): void {
				switch (_0.kind) {
					case "named":
						$(ctx.names.serialize(varint))(0, output);
						$(ctx.names.serialize(nty))(_0.value, output);
						return;
					case "ids":
						$(ctx.names.serialize(varint))(_0.value.length + 1, output);
						for (const id of _0.value) {
							$(ctx.names.serialize(varint))(id, output);
						}
						return;
				}
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				const n = $(ctx.names.deserialize(varint))(input);
				if (n === 0) {
					return { kind: "named", value: $read_named };
				}
				const count = n - 1;
				if (count < 0 || count > input.remaining()) {
					throw new $perror("InvalidValue", "invalid id set length");
				}
				const ids = $new_ids;
				for (let i = 0; i < count; i++) {
					ids[i] = $(ctx.names.deserialize(varint))(input);
				}
				return { kind: "ids", value: ids };
			}
			export function size_of(_0: Ty): number {
				switch (_0.kind) {
					case "named":
						return 1 + $(ctx.names.size_of(nty))(_0.value);
					case "ids": {
						let n = $(ctx.names.size_of(varint))(_0.value.length + 1);
						for (const id of _0.value) {
							n += $(ctx.names.size_of(varint))(id);
						}
						return n;
					}
				}
			}
		})
	}
}

impl NativeCodegen<JavaScript> for TopBitSetTerminatedArray {
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		construct_array(cx, native, c)
	}
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<JavaScript>,
	) -> Option<Tokens> {
		Some(array_ty(names, value_of(native)?))
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let value_use = ctx
			.specialization_use(VALUE)
			.context("topBitSetTerminatedArray value")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("topBitSetTerminatedArray type")?;
		let (pinput, poutput, perror) = (
			&pds(ctx.names, "Input"),
			&pds(ctx.names, "Output"),
			&pds(ctx.names, "PdsError"),
		);
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = $ty;
			export function serialize(_0: Ty, output: $poutput): void {
				if (_0.length === 0) {
					throw new $perror("InvalidValue", "top bit terminated array cannot be empty");
				}
				for (let i = 0; i < _0.length; i++) {
					const item = new $poutput();
					$(ctx.names.serialize(value))(_0[i], item);
					const bytes = item.into_inner();
					if (bytes.length === 0 || (bytes[0] & 0x80) !== 0) {
						throw new $perror("InvalidValue", "element must start with a 7-bit byte");
					}
					if (i + 1 < _0.length) {
						bytes[0] |= 0x80;
					}
					output.write(bytes);
				}
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				const out: Array<$(ctx.names.ty(value))> = [];
				while (true) {
					const more = (input.peek() & 0x80) !== 0;
					input.mask_next(0x7f);
					out.push($(ctx.deserialize_call(value_use)));
					if (!more) {
						return $(from_list(ctx, value));
					}
				}
			}
			export function size_of(_0: Ty): number {
				let n = 0;
				for (const e of _0) {
					n += $(ctx.names.size_of(value))(e);
				}
				return n;
			}
		})
	}
}
