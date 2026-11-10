use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::{js, JavaScript};
use genco::prelude::*;

use super::{
	bitfield_entries, specialization_use, Array, Bitfield, Bitflags, Buffer, EntityMetadataLoop,
	Names, NativeCodegen, NativeCodegenCtx, OptionP, PString, Registry, RegistryEntryHolder,
	RegistryEntryHolderSet, Scalar, TopBitSetTerminatedArray, VALUE,
};
use crate::ast::Ident;
use crate::generator::common::EqKind;
use crate::hir::{self, TypeId};
use crate::mir;

type Tokens = js::Tokens;
type Ctx<'a> = NativeCodegenCtx<'a, JavaScript>;

pub fn register(reg: &mut Registry) {
	reg.register_codegen::<JavaScript>("bitfield", Bitfield);
	reg.register_codegen::<JavaScript>("bitflags", Bitflags);
	reg.register_codegen::<JavaScript>("array", Array);
	reg.register_codegen::<JavaScript>("option", OptionP);
	reg.register_codegen::<JavaScript>("buffer", Buffer);
	reg.register_codegen::<JavaScript>("pstring", PString);
	reg.register_codegen::<JavaScript>("entityMetadataLoop", EntityMetadataLoop);
	reg.register_codegen::<JavaScript>("terminated", EntityMetadataLoop);
	reg.register_codegen::<JavaScript>("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	reg.register_codegen::<JavaScript>("registryEntryHolder", RegistryEntryHolder);
	reg.register_codegen::<JavaScript>("registryEntryHolderSet", RegistryEntryHolderSet);
}

fn pds(name: &str) -> js::Import {
	js::import("../pds.ts", name)
}

fn typed_array(scalar: Scalar) -> &'static str {
	match scalar {
		Scalar::I8 => "Int8Array",
		Scalar::U8 => "Uint8Array",
		Scalar::I16 => "Int16Array",
		Scalar::U16 => "Uint16Array",
		Scalar::I32 => "Int32Array",
		Scalar::U32 => "Uint32Array",
		Scalar::I64 => "BigInt64Array",
		Scalar::U64 => "BigUint64Array",
		Scalar::F32 => "Float32Array",
		Scalar::F64 => "Float64Array",
	}
}

fn array_ty(names: &dyn Names<JavaScript>, value: TypeId) -> Tokens {
	match names.scalar(value) {
		Some(s) => quote!($(typed_array(s))),
		None => quote!(Array<$(names.ty(value))>),
	}
}

fn wide(names: &dyn Names<JavaScript>, ty: TypeId) -> bool {
	matches!(names.scalar(ty), Some(Scalar::I64 | Scalar::U64))
}

fn int_as(names: &dyn Names<JavaScript>, ty: TypeId, expr: Tokens) -> Tokens {
	if wide(names, ty) {
		quote!(BigInt($expr))
	} else {
		expr
	}
}

fn value_of(native: &mir::NativeType) -> Option<TypeId> {
	specialization_use(native, VALUE).map(|u| u.target)
}

fn collect(ctx: &Ctx, value: TypeId, count: Tokens, read: Tokens) -> Tokens {
	let perror = &pds("PdsError");
	match ctx.names.scalar(value) {
		Some(s) => quote! {
			if ($(&count) > input.remaining()) {
				throw new $perror("NotEnoughData", "array length exceeds the input");
			}
			const out = new $(typed_array(s))($(&count));
			for (let i = 0; i < $(&count); i++) {
				out[i] = $read;
			}
			return out;
		},
		None => quote! {
			const out: Ty = [];
			for (let i = 0; i < $(&count); i++) {
				out.push($read);
			}
			return out;
		},
	}
}

fn read_count(ctx: &Ctx, expr: Tokens) -> Tokens {
	let perror = &pds("PdsError");
	let _ = ctx;
	quote! {
		const count = Number($expr);
		if (!Number.isSafeInteger(count) || count < 0) {
			throw new $perror("InvalidValue", "invalid length");
		}
	}
}

impl NativeCodegen<JavaScript> for Array {
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
		let inner_a = if ctx.boxed(&slot) {
			quote!(a.value)
		} else {
			quote!(a)
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
			$(for (t, _) in ctx.eq_targets.iter().filter(|(_, k)| *k != EqKind::Bool) join($['\r']) =>
				export function $(format!("is_equal_{}", t.raw()))(a: Ty, b: $(ctx.names.ty(*t))): boolean {
					return a !== null && $(&inner_a) === b;
				}
			)
			$(if ctx.eq_targets.iter().any(|(_, k)| *k == EqKind::Bool) =>
				export function is_equal_bool(a: Ty, b: boolean): boolean {
					return a === b;
				}
			)
		})
	}
}

impl NativeCodegen<JavaScript> for Buffer {
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

impl NativeCodegen<JavaScript> for RegistryEntryHolder {
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolder needs the `varint` native"))?;
		let Some((iname, ity_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolder needs an inline field from its body");
		};
		let ity = ity_use.target;
		let read_inline = ctx.slot_new(iname, ctx.deserialize_call(ity_use));
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
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
	fn generate(&self, ctx: &Ctx) -> Result<Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolderSet needs the `varint` native"))?;
		let Some((nname, nty_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolderSet needs a named field from its body");
		};
		let nty = nty_use.target;
		let read_named = ctx.slot_new(nname, ctx.deserialize_call(nty_use));
		let (pinput, poutput, perror) = (&pds("Input"), &pds("Output"), &pds("PdsError"));
		let ids_ty = array_ty(ctx.names, varint);
		let new_ids = match ctx.names.scalar(varint) {
			Some(s) => quote!(new $(typed_array(s))(count)),
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

fn from_list(ctx: &Ctx, value: TypeId) -> Tokens {
	match ctx.names.scalar(value) {
		Some(s) => quote!($(typed_array(s)).from(out)),
		None => quote!(out),
	}
}

impl NativeCodegen<JavaScript> for EntityMetadataLoop {
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
			.context("terminated array value")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("terminated array type")?;
		let end = match ctx
			.native
			.const_args
			.iter()
			.find(|(n, _)| n.as_str() == "end")
		{
			Some((_, hir::Value::Number(n))) => {
				u8::try_from(*n).map_err(|_| anyhow!("terminator {n} does not fit a byte"))?
			}
			other => bail!("terminated array needs a constant byte `end`, got {other:?}"),
		};
		let end = end.to_string();
		let u8id =
			(ctx.resolve)("u8").ok_or_else(|| anyhow!("terminated array needs the `u8` native"))?;
		let (pinput, poutput) = (&pds("Input"), &pds("Output"));
		Ok(quote! {
			$((ctx.args_struct)())
			export type Ty = $ty;
			export function serialize(_0: Ty, output: $poutput): void {
				for (const e of _0) {
					$(ctx.names.serialize(value))(e, output);
				}
				$(ctx.names.serialize(u8id))($(&end), output);
			}
			export function deserialize(input: $pinput$(if !ctx.params.is_empty() => , $((ctx.args_param)()))): Ty {
				const out: Array<$(ctx.names.ty(value))> = [];
				while (input.peek() !== $(&end)) {
					out.push($(ctx.deserialize_call(value_use)));
				}
				input.skip();
				return $(from_list(ctx, value));
			}
			export function size_of(_0: Ty): number {
				let n = 1;
				for (const e of _0) {
					n += $(ctx.names.size_of(value))(e);
				}
				return n;
			}
		})
	}
}

impl NativeCodegen<JavaScript> for TopBitSetTerminatedArray {
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
		let (pinput, poutput, perror) = (&pds("Input"), &pds("Output"), &pds("PdsError"));
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
