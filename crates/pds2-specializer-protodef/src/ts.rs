use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::JavaScript;
use genco::prelude::*;

use pds2_core::ast::Ident;
use pds2_core::hir::TypeId;
use pds2_core::mir;
use pds2_core::specializer::ts::{
	array_ty, collect, construct_array, int_as, pds, read_count, value_of, wide, Ctx,
	Tokens,
};
use pds2_core::specializer::{LowerCx, Names, NativeCodegen, VALUE};

use crate::natives::{bitfield_entries, Array, Bitfield, Bitflags, Buffer, OptionP, PString};

impl NativeCodegen<JavaScript> for Array {
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
			.context("array element type")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("array type")?;
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		let read = ctx.deserialize_call(value_use);
		let main = match ctx.variant.as_str() {
			"inline" => {
				let count = ctx.specialization("Count").context("array count type")?;
				quote! {
					export function serialize(_0: Ty, output: $poutput): void {
						$(ctx.names.serialize(count))($(int_as(ctx.names, count, quote!(_0.length))), output);
						for (const e of _0) {
							$(ctx.names.serialize(value))(e, output);
						}
					}
					export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
						$(read_count(ctx, quote!($(ctx.names.deserialize(count))(input))))
						$(collect(ctx, value, quote!(count), read))
					}
					export function size_of(_0: Ty): number {
						let n = $(ctx.names.size_of(count))($(int_as(ctx.names, count, quote!(_0.length))));
						for (const e of _0) {
							n += $(ctx.names.size_of(value))(e);
						}
						return n;
					}
				}
			}
			"sized" => {
				if ctx.params.is_empty() {
					bail!("array::sized needs a `count` argument");
				}
				quote! {
					export function serialize(_0: Ty, output: $poutput): void {
						for (const e of _0) {
							$(ctx.names.serialize(value))(e, output);
						}
					}
					export function deserialize(input: $pinput, $((ctx.args_param)())): Ty {
						$(read_count(ctx, quote!(args.count)))
						$(collect(ctx, value, quote!(count), read))
					}
					export function size_of(_0: Ty): number {
						let n = 0;
						for (const e of _0) {
							n += $(ctx.names.size_of(value))(e);
						}
						return n;
					}
				}
			}
			v => bail!("unknown array overload: {v:?}"),
		};
		let virtual_size = match ctx.virtual_field("size") {
			Some(virt) => quote! {
				export function virtual_size(_0: Ty): $(ctx.names.ty(virt)) {
					return $(int_as(ctx.names, virt, quote!(_0.length)));
				}
			},
			None => quote!(),
		};
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = $ty;
			$main
			$virtual_size
		})
	}
}

impl NativeCodegen<JavaScript> for OptionP {
	fn nullable(&self, _native: &mir::NativeType) -> bool {
		true
	}
	fn wraps_nullable(&self, _native: &mir::NativeType, slot: &Ident) -> bool {
		slot.as_str() == VALUE
	}
	fn inline_ty(
		&self,
		id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<JavaScript>,
	) -> Option<Tokens> {
		let value = value_of(native)?;
		Some(quote!($(names.slot_ty(id, &Ident::new(VALUE), value)) | null))
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let value_use = ctx.specialization_use(VALUE).context("option value type")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("option type")?;
		let slot = Ident::new(VALUE);
		let inner = if ctx.boxed(&slot) {
			quote!(_0.value)
		} else {
			quote!(_0)
		};
		let read = ctx.slot_new(&slot, ctx.deserialize_call(value_use));
		let boolean =
			(ctx.resolve)("bool").ok_or_else(|| anyhow!("option needs the `bool` native"))?;
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = $ty;
			export function serialize(_0: Ty, output: $poutput): void {
				$(ctx.names.serialize(boolean))(_0 !== null, output);
				if (_0 !== null) {
					$(ctx.names.serialize(value))($(&inner), output);
				}
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				return $(ctx.names.deserialize(boolean))(input) ? $read : null;
			}
			export function size_of(_0: Ty): number {
				let n = $(ctx.names.size_of(boolean))(_0 !== null);
				if (_0 !== null) {
					n += $(ctx.names.size_of(value))($(&inner));
				}
				return n;
			}
		})
	}
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		let mir::Construction::Wrap(inner) = c else {
			return Ok(quote!(null));
		};
		Ok(cx.names.slot_new(id, &Ident::new(VALUE), (cx.lower)(inner)?))
	}
}

impl NativeCodegen<JavaScript> for Buffer {
	fn construct(
		&self,
		_cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		pds2_core::specializer::reflect::construct_bytes_js(c)
	}
	fn inline_ty(
		&self,
		_id: TypeId,
		_native: &mir::NativeType,
		_names: &dyn Names<JavaScript>,
	) -> Option<Tokens> {
		Some(quote!(Uint8Array))
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		let main = match ctx.variant.as_str() {
			"inline" => {
				let count = ctx.specialization("Count").context("buffer count type")?;
				quote! {
					export function serialize(_0: Ty, output: $poutput): void {
						$(ctx.names.serialize(count))($(int_as(ctx.names, count, quote!(_0.length))), output);
						output.write(_0);
					}
					export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
						$(read_count(ctx, quote!($(ctx.names.deserialize(count))(input))))
						return input.read_bytes(count);
					}
					export function size_of(_0: Ty): number {
						return $(ctx.names.size_of(count))($(int_as(ctx.names, count, quote!(_0.length)))) + _0.length;
					}
				}
			}
			"sized" => quote! {
				export function serialize(_0: Ty, output: $poutput): void {
					output.write(_0);
				}
				export function deserialize(input: $pinput, $((ctx.args_param)())): Ty {
					$(read_count(ctx, quote!(args.count)))
					return input.read_bytes(count);
				}
				export function size_of(_0: Ty): number {
					return _0.length;
				}
			},
			v => bail!("unknown buffer overload: {v:?}"),
		};
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = Uint8Array;
			$main
		})
	}
}

impl NativeCodegen<JavaScript> for PString {
	fn construct(
		&self,
		_cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		Ok(quote!($(quoted(pds2_core::specializer::reflect::string_of(c)?))))
	}
	fn inline_ty(
		&self,
		_id: TypeId,
		_native: &mir::NativeType,
		_names: &dyn Names<JavaScript>,
	) -> Option<Tokens> {
		Some(quote!(string))
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let count = ctx.specialization("Count").context("pstring count type")?;
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		let (encode, decode) = (&pds("utf8_encode"), &pds("utf8_decode"));
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = string;
			export function serialize(_0: Ty, output: $poutput): void {
				const bytes = $encode(_0);
				$(ctx.names.serialize(count))($(int_as(ctx.names, count, quote!(bytes.length))), output);
				output.write(bytes);
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				$(read_count(ctx, quote!($(ctx.names.deserialize(count))(input))))
				return $decode(input.read_bytes(count));
			}
			export function size_of(_0: Ty): number {
				const len = $encode(_0).length;
				return $(ctx.names.size_of(count))($(int_as(ctx.names, count, quote!(len)))) + len;
			}
		})
	}
}

impl NativeCodegen<JavaScript> for Bitfield {
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		pds2_core::specializer::reflect::construct_object(cx, c)
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let entries = bitfield_entries(&ctx.native.body)?;
		let total: u32 = entries.iter().map(|e| e.size).sum();
		if total % 8 != 0 {
			bail!("bitfield does not pack into whole bytes: {total} bits");
		}
		let bytes = (total / 8).to_string();
		let u8id = (ctx.resolve)("u8").ok_or_else(|| anyhow!("bitfield needs the `u8` native"))?;
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		let mut pack = Vec::new();
		let mut unpack = Vec::new();
		let mut offset = 0;
		for e in &entries {
			let shift = total - offset - e.size;
			let mask = format!("{:#x}n", (1u128 << e.size) - 1);
			let name = e.name.as_str();
			pack.push(
				quote!(acc = (acc << $(format!("{}n", e.size))) | (BigInt(_0.$name) & $(&mask));),
			);
			let raw = quote!((acc >> $(format!("{shift}n"))) & $(&mask));
			let raw = if e.signed {
				quote!(BigInt.asIntN($(e.size.to_string()), $raw))
			} else {
				raw
			};
			let value = if e.bits == 64 {
				raw
			} else {
				quote!(Number($raw))
			};
			unpack.push(quote!($name: $value,));
			offset += e.size;
		}
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = {
				$(for (e, (_, ty)) in entries.iter().zip(ctx.native.fields.iter()) join($['\r']) =>
					$(e.name.as_str()): $(ctx.names.ty(ty.target));
				)
			};
			export function serialize(_0: Ty, output: $poutput): void {
				let acc = 0n;
				$(for p in &pack join($['\r']) => $p)
				for (let i = $(&bytes) - 1; i >= 0; i--) {
					$(ctx.names.serialize(u8id))(Number((acc >> BigInt(i * 8)) & 0xffn), output);
				}
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				let acc = 0n;
				for (let i = 0; i < $(&bytes); i++) {
					acc = (acc << 8n) | BigInt($(ctx.names.deserialize(u8id))(input));
				}
				return {
					$(for u in &unpack join($['\r']) => $u)
				};
			}
			export function size_of(_0: Ty): number {
				return $(&bytes);
			}
		})
	}
}

impl NativeCodegen<JavaScript> for Bitflags {
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens> {
		pds2_core::specializer::reflect::construct_object(cx, c)
	}
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let value = ctx.specialization(VALUE)?;
		let bit = |i: usize| format!("{:#x}n", 1u64 << i);
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		let packed = if wide(ctx.names, value) {
			quote!(pack(_0))
		} else {
			quote!(Number(pack(_0)))
		};
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = {
				$(for (name, ty) in ctx.native.fields.iter() join($['\r']) =>
					$(name.as_str()): $(ctx.names.ty(ty.target));
				)
			};
			function pack(_0: Ty): bigint {
				let acc = 0n;
				$(for (i, (name, _)) in ctx.native.fields.iter().enumerate() join($['\r']) =>
					if (_0.$(name.as_str())) {
						acc |= $(bit(i));
					}
				)
				return acc;
			}
			export function serialize(_0: Ty, output: $poutput): void {
				$(ctx.names.serialize(value))($(&packed), output);
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				const acc = BigInt($(ctx.names.deserialize(value))(input));
				return {
					$(for (i, (name, _)) in ctx.native.fields.iter().enumerate() join($['\r']) =>
						$(name.as_str()): (acc & $(bit(i))) !== 0n,
					)
				};
			}
			export function size_of(_0: Ty): number {
				return $(ctx.names.size_of(value))($(&packed));
			}
		})
	}
}
