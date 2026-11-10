use std::collections::BTreeSet;

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use super::encode::{int_of, Args, Encoder, Encoding, Val};
use super::expr::{describe, Elaborator, ValueModel};
use super::{
	bitfield_entries, specialization_use, Array, Bitfield, Bitflags, Buffer, EntityMetadataLoop,
	LowerCx, OptionP, PString, RegistryEntryHolder, RegistryEntryHolderSet,
	TopBitSetTerminatedArray, VALUE,
};
use crate::ast::Ident;
use crate::generator::rust::rname;
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Value};

fn use_of<'n>(native: &'n mir::NativeType, name: &str) -> Result<&'n mir::Type> {
	specialization_use(native, name).ok_or_else(|| anyhow!("generic {name:?} is not bound"))
}

fn list_of(c: &Construction) -> Result<&[Expr]> {
	match c {
		Construction::List(items) => Ok(items),
		c => bail!("expected a list, got {c:?}"),
	}
}

fn record_of(c: &Construction) -> Result<&[(Ident, Expr)]> {
	match c {
		Construction::Record(fields) => Ok(fields),
		c => bail!("expected a record, got {c:?}"),
	}
}

fn variant_of(c: &Construction) -> Result<(&Ident, &[Expr])> {
	match c {
		Construction::Variant(n, p) => Ok((n, p)),
		c => bail!("expected a variant, got {c:?}"),
	}
}

fn count_arg(v: &Val, len: usize) -> Result<()> {
	if let Some(count) = v.args.get(&Ident::new("count")) {
		let n = int_of(count).ok_or_else(|| anyhow!("count {} is not a number", describe(count)))?;
		ensure!(
			n == len as i128,
			"{len} items, but the count argument says {n}"
		);
	}
	Ok(())
}

fn encode_items(cx: &Encoding, v: &Val, items: &[Expr], out: &mut Vec<u8>) -> Result<()> {
	let value = use_of(v.native, VALUE)?;
	let scope = v.scope();
	for (i, item) in items.iter().enumerate() {
		cx.encode_use(value, item, &scope, out)
			.with_context(|| format!("item {i}"))?;
	}
	Ok(())
}

fn encode_bytes(cx: &Encoding, bytes: &[u8], out: &mut Vec<u8>) -> Result<()> {
	let u8id = cx.resolve_name("u8")?;
	for b in bytes {
		cx.encode_number(u8id, i128::from(*b), out)?;
	}
	Ok(())
}

fn bytes_of(c: &Construction) -> Result<&[u8]> {
	match c {
		Construction::Literal(Value::Bytes(b)) => Ok(b),
		Construction::Literal(Value::String(s)) => Ok(s.as_bytes()),
		c => bail!("expected bytes, got {c:?}"),
	}
}

fn variant_name<'v>(v: &'v Val) -> &'v str {
	v.native
		.specializer
		.as_ref()
		.map_or("default", |(_, variant)| variant.as_str())
}

impl Encoder for OptionP {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let boolean = cx.resolve_name("bool")?;
		let present = matches!(v.c, Construction::Wrap(_));
		cx.encode(
			&cx.elab.build(boolean, &Expr::Const(Value::Bool(present)))?,
			&Args::new(),
			out,
		)?;
		if let Construction::Wrap(inner) = v.c {
			cx.encode_use(use_of(v.native, VALUE)?, inner, &v.scope(), out)?;
		}
		Ok(())
	}
}

impl Encoder for Array {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let items = list_of(v.c)?;
		if variant_name(v) == "inline" {
			let count = use_of(v.native, "Count")?;
			let n = cx.number(count.target, items.len() as i128)?;
			cx.encode_use(count, &n, &v.scope(), out).context("count")?;
		} else {
			count_arg(v, items.len())?;
		}
		encode_items(cx, v, items, out)
	}
	fn virtual_field(&self, cx: &Encoding, v: &Val, name: &Ident) -> Result<Expr> {
		let (_, ty) = v
			.native
			.virtual_fields
			.iter()
			.find(|(n, _)| n == name)
			.ok_or_else(|| anyhow!("{} has no virtual field {name:?}", cx.name(v.id)))?;
		ensure!(name.as_str() == "size", "unknown array virtual field {name:?}");
		cx.number(ty.target, list_of(v.c)?.len() as i128)
	}
}

impl Encoder for EntityMetadataLoop {
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

impl Encoder for TopBitSetTerminatedArray {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let items = list_of(v.c)?;
		ensure!(!items.is_empty(), "a top-bit terminated array needs at least one item");
		let value = use_of(v.native, VALUE)?;
		for (i, item) in items.iter().enumerate() {
			let mut buf = Vec::new();
			cx.encode_use(value, item, &v.scope(), &mut buf)
				.with_context(|| format!("item {i}"))?;
			let first = buf
				.first_mut()
				.ok_or_else(|| anyhow!("item {i} encodes to no bytes"))?;
			ensure!(*first & 0x80 == 0, "item {i} already sets the top bit");
			if i + 1 < items.len() {
				*first |= 0x80;
			}
			out.extend(buf);
		}
		Ok(())
	}
}

impl ValueModel for TopBitSetTerminatedArray {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		super::list_value(cx, id, native, value)
	}
}

fn record_fields(
	cx: &Elaborator,
	id: TypeId,
	fields: &[(Ident, mir::Type)],
	value: &Expr,
) -> Result<Construction> {
	let Expr::Record(given) = value else {
		bail!("{} is written as a record `(field: value, ...)`", cx.name(id));
	};
	for (n, _) in given {
		ensure!(
			fields.iter().any(|(f, _)| f == n),
			"{} has no field {n:?}",
			cx.name(id)
		);
	}
	let mut out = Vec::new();
	for (name, ty) in fields {
		let (_, e) = given
			.iter()
			.find(|(n, _)| n == name)
			.ok_or_else(|| anyhow!("{} needs field {name:?}", cx.name(id)))?;
		let e = cx
			.coerce(e.clone(), ty.target)
			.with_context(|| format!("field {name:?}"))?;
		out.push((name.clone(), e));
	}
	Ok(Construction::Record(out))
}

pub fn construct_struct(cx: &LowerCx<Rust>, id: TypeId, c: &Construction) -> Result<rust::Tokens> {
	let mut parts = Vec::new();
	for (n, e) in record_of(c)? {
		parts.push(quote!($(rname(n)): $((cx.lower)(e)?)));
	}
	Ok(quote!($(cx.names.ty(id)) { $(for p in parts join(, ) => $p) }))
}

pub fn construct_object(cx: &LowerCx<JavaScript>, c: &Construction) -> Result<js::Tokens> {
	let mut parts = Vec::new();
	for (n, e) in record_of(c)? {
		parts.push(quote!($(n.as_str()): $((cx.lower)(e)?)));
	}
	Ok(quote!({ $(for p in parts join(, ) => $p) }))
}

impl ValueModel for Bitfield {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		record_fields(cx, id, &native.fields, value)
	}
}

impl Encoder for Bitfield {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let entries = bitfield_entries(&v.native.body)?;
		let given = record_of(v.c)?;
		let total: u32 = entries.iter().map(|e| e.size).sum();
		ensure!(total % 8 == 0, "bitfield does not pack into whole bytes: {total} bits");
		let mut acc: u128 = 0;
		for e in &entries {
			let (_, value) = given
				.iter()
				.find(|(n, _)| *n == e.name)
				.ok_or_else(|| anyhow!("bitfield value lacks {:?}", e.name))?;
			let n = int_of(value).ok_or_else(|| anyhow!("{:?} is not a number", e.name))?;
			let range = if e.signed {
				-(1i128 << (e.size - 1))..=(1i128 << (e.size - 1)) - 1
			} else {
				0..=(1i128 << e.size) - 1
			};
			ensure!(range.contains(&n), "{:?} = {n} does not fit {} bits", e.name, e.size);
			acc = (acc << e.size) | ((n as u128) & ((1u128 << e.size) - 1));
		}
		let bytes: Vec<u8> = (0..total / 8).rev().map(|i| (acc >> (i * 8)) as u8).collect();
		encode_bytes(cx, &bytes, out)
	}
}

impl ValueModel for Bitflags {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let Expr::List(items) = value else {
			bail!("{} is written as a list of set flags `[flag, ...]`", cx.name(id));
		};
		let mut set = BTreeSet::new();
		for item in items {
			let Expr::Const(Value::UnionDescriminator(flag)) = item else {
				bail!("{} is not a flag name", describe(item));
			};
			ensure!(
				native.fields.iter().any(|(n, _)| n == flag),
				"{} has no flag {flag:?}",
				cx.name(id)
			);
			ensure!(set.insert(flag.clone()), "flag {flag:?} is set twice");
		}
		let fields = native
			.fields
			.iter()
			.map(|(n, ty)| {
				let on = cx.build(ty.target, &Expr::Const(Value::Bool(set.contains(n))))?;
				Ok((n.clone(), on))
			})
			.collect::<Result<_>>()?;
		Ok(Construction::Record(fields))
	}
}

impl Encoder for Bitflags {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let mut acc: i128 = 0;
		for (i, (n, _)) in v.native.fields.iter().enumerate() {
			let (_, flag) = record_of(v.c)?
				.iter()
				.find(|(f, _)| f == n)
				.ok_or_else(|| anyhow!("bitflags value lacks {n:?}"))?;
			if int_of(flag) == Some(1) {
				acc |= 1 << i;
			}
		}
		let value = use_of(v.native, VALUE)?;
		cx.encode_use(value, &cx.number(value.target, acc)?, &v.scope(), out)
	}
}

impl ValueModel for Buffer {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		_native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		match value {
			Expr::Const(Value::Bytes(b)) => Ok(Construction::Literal(Value::Bytes(b.clone()))),
			Expr::Const(Value::String(s)) => {
				Ok(Construction::Literal(Value::Bytes(s.as_bytes().to_vec())))
			}
			v => bail!("{} is written as bytes `b\"...\"`, got {}", cx.name(id), describe(v)),
		}
	}
}

impl Encoder for Buffer {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let bytes = bytes_of(v.c)?;
		if variant_name(v) == "inline" {
			let count = use_of(v.native, "Count")?;
			let n = cx.number(count.target, bytes.len() as i128)?;
			cx.encode_use(count, &n, &v.scope(), out).context("count")?;
		} else {
			count_arg(v, bytes.len())?;
		}
		encode_bytes(cx, bytes, out)
	}
}

pub fn construct_bytes_rust(c: &Construction) -> Result<rust::Tokens> {
	let bytes = bytes_of(c)?;
	Ok(quote!(vec![$(for b in bytes join(, ) => $(format!("{b}_u8")))]))
}

pub fn construct_bytes_js(c: &Construction) -> Result<js::Tokens> {
	let bytes = bytes_of(c)?.iter().map(u8::to_string).collect::<Vec<_>>().join(", ");
	Ok(quote!(new Uint8Array([$bytes])))
}

impl ValueModel for PString {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		_native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		match value {
			Expr::Const(Value::String(s)) => Ok(Construction::Literal(Value::String(s.clone()))),
			v => bail!("{} is written as a string, got {}", cx.name(id), describe(v)),
		}
	}
}

impl Encoder for PString {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let bytes = bytes_of(v.c)?;
		let count = use_of(v.native, "Count")?;
		let n = cx.number(count.target, bytes.len() as i128)?;
		cx.encode_use(count, &n, &v.scope(), out).context("length")?;
		encode_bytes(cx, bytes, out)
	}
}

pub fn string_of(c: &Construction) -> Result<&str> {
	match c {
		Construction::Literal(Value::String(s)) => Ok(s),
		c => bail!("expected a string, got {c:?}"),
	}
}

fn holder_value(
	cx: &Elaborator,
	id: TypeId,
	native: &mir::NativeType,
	value: &Expr,
	list_key: &str,
	list_kind: &str,
	body_kind: &str,
) -> Result<Construction> {
	let Expr::Record(given) = value else {
		bail!("{} is written as `({list_key}: ...)` or `(<field>: ...)`", cx.name(id));
	};
	let [(key, e)] = given.as_slice() else {
		bail!("{} value names one representation, got {} fields", cx.name(id), given.len());
	};
	let varint = cx.resolve_name("varint")?;
	if key.as_str() == list_key {
		let payload = match list_kind {
			"ids" => {
				let Expr::List(items) = e else {
					bail!("`{list_key}` is written as a list `[...]`");
				};
				Expr::List(
					items
						.iter()
						.map(|i| cx.coerce(i.clone(), varint))
						.collect::<Result<_>>()?,
				)
			}
			_ => cx.coerce(e.clone(), varint)?,
		};
		return Ok(Construction::Variant(Ident::new(list_kind), vec![payload]));
	}
	let (name, ty) = native
		.fields
		.first()
		.ok_or_else(|| anyhow!("{} has no body field", cx.name(id)))?;
	ensure!(
		key == name,
		"{} is written as `({list_key}: ...)` or `({name:?}: ...)`, got {key:?}",
		cx.name(id)
	);
	let payload = cx
		.coerce(e.clone(), ty.target)
		.with_context(|| format!("field {name:?}"))?;
	Ok(Construction::Variant(Ident::new(body_kind), vec![payload]))
}

impl ValueModel for RegistryEntryHolder {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		holder_value(cx, id, native, value, "id", "id", "inline")
	}
}

impl Encoder for RegistryEntryHolder {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let varint = cx.resolve_name("varint")?;
		match variant_of(v.c)? {
			(k, [id]) if k.as_str() == "id" => {
				let n = int_of(id).ok_or_else(|| anyhow!("id {} is not a number", describe(id)))?;
				cx.encode_number(varint, n + 1, out)
			}
			(_, [value]) => {
				cx.encode_number(varint, 0, out)?;
				let (_, ty) = v.native.fields.first().ok_or_else(|| anyhow!("no body field"))?;
				cx.encode_use(ty, value, &v.scope(), out)
			}
			(k, p) => bail!("registryEntryHolder variant {k:?} with {} values", p.len()),
		}
	}
}

impl ValueModel for RegistryEntryHolderSet {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		holder_value(cx, id, native, value, "ids", "ids", "named")
	}
}

impl Encoder for RegistryEntryHolderSet {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let varint = cx.resolve_name("varint")?;
		match variant_of(v.c)? {
			(k, [Expr::List(ids)]) if k.as_str() == "ids" => {
				cx.encode_number(varint, ids.len() as i128 + 1, out)?;
				for id in ids {
					cx.encode(id, &Args::new(), out)?;
				}
				Ok(())
			}
			(_, [value]) => {
				cx.encode_number(varint, 0, out)?;
				let (_, ty) = v.native.fields.first().ok_or_else(|| anyhow!("no body field"))?;
				cx.encode_use(ty, value, &v.scope(), out)
			}
			(k, p) => bail!("registryEntryHolderSet variant {k:?} with {} values", p.len()),
		}
	}
}

fn holder_slot(native: &mir::NativeType) -> Result<&Ident> {
	native
		.fields
		.first()
		.map(|(n, _)| n)
		.ok_or_else(|| anyhow!("holder without a body field"))
}

pub fn construct_holder_rust(
	cx: &LowerCx<Rust>,
	id: TypeId,
	native: &mir::NativeType,
	c: &Construction,
) -> Result<rust::Tokens> {
	let (kind, payload) = variant_of(c)?;
	let ty = cx.names.ty(id);
	Ok(match (kind.as_str(), payload) {
		("id", [n]) => quote!($ty::Id($((cx.lower)(n)?))),
		("ids", [list]) => quote!($ty::Ids($((cx.lower)(list)?))),
		("inline", [v]) => {
			quote!($ty::Inline($(cx.names.slot_new(id, holder_slot(native)?, (cx.lower)(v)?))))
		}
		("named", [v]) => {
			quote!($ty::Named($(cx.names.slot_new(id, holder_slot(native)?, (cx.lower)(v)?))))
		}
		(k, p) => bail!("holder variant {k:?} with {} values", p.len()),
	})
}

pub fn holder_payload_js(
	cx: &LowerCx<JavaScript>,
	c: &Construction,
	typed_ids: Option<&'static str>,
) -> Result<(String, js::Tokens)> {
	let (kind, payload) = variant_of(c)?;
	let [value] = payload else {
		bail!("holder variant {kind:?} with {} values", payload.len());
	};
	let value = match (kind.as_str(), value, typed_ids) {
		("ids", Expr::List(items), Some(array)) => {
			let items = items
				.iter()
				.map(|i| (cx.lower)(i))
				.collect::<Result<Vec<_>>>()?;
			quote!(new $array([$(for i in items join(, ) => $i)]))
		}
		_ => (cx.lower)(value)?,
	};
	Ok((kind.as_str().to_owned(), value))
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use crate::specializer::{register_protodef_types, Registry};

	#[test]
	fn every_specializer_registers_an_encoder() {
		let mut reg = Registry::default();
		register_protodef_types(&mut reg);
		let missing: BTreeSet<&String> = reg
			.asthir
			.keys()
			.chain(reg.layout.keys())
			.chain(reg.codegen_rust.keys())
			.chain(reg.codegen_ts.keys())
			.chain(reg.value.keys())
			.filter(|n| reg.encoder(n).is_none())
			.collect();
		assert!(missing.is_empty(), "no encoder for {missing:?}");
	}
}
