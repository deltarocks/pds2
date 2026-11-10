use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use super::decode::Input;
use super::encode::{int_of, Args, Encoding, Target, Val};
use super::expr::{describe, Elaborator};
use super::{specialization_use, LowerCx, VALUE};
use crate::ast::Ident;
use crate::generator::rust::rname;
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Value};

pub fn use_of<'n>(native: &'n mir::NativeType, name: &str) -> Result<&'n mir::Type> {
	specialization_use(native, name).ok_or_else(|| anyhow!("generic {name:?} is not bound"))
}

pub fn list_of(c: &Construction) -> Result<&[Expr]> {
	match c {
		Construction::List(items) => Ok(items),
		c => bail!("expected a list, got {c:?}"),
	}
}

pub fn record_of(c: &Construction) -> Result<&[(Ident, Expr)]> {
	match c {
		Construction::Record(fields) => Ok(fields),
		c => bail!("expected a record, got {c:?}"),
	}
}

pub fn variant_of(c: &Construction) -> Result<(&Ident, &[Expr])> {
	match c {
		Construction::Variant(n, p) => Ok((n, p)),
		c => bail!("expected a variant, got {c:?}"),
	}
}

pub fn count_arg(v: &Val, len: usize) -> Result<()> {
	if let Some(count) = v.args.get(&Ident::new("count")) {
		let n = int_of(count).ok_or_else(|| anyhow!("count {} is not a number", describe(count)))?;
		ensure!(
			n == len as i128,
			"{len} items, but the count argument says {n}"
		);
	}
	Ok(())
}

pub fn encode_items(cx: &Encoding, v: &Val, items: &[Expr], out: &mut Vec<u8>) -> Result<()> {
	let value = use_of(v.native, VALUE)?;
	let scope = v.scope();
	for (i, item) in items.iter().enumerate() {
		cx.encode_use(value, item, &scope, out)
			.with_context(|| format!("item {i}"))?;
	}
	Ok(())
}

pub fn encode_bytes(cx: &Encoding, bytes: &[u8], out: &mut Vec<u8>) -> Result<()> {
	let u8id = cx.resolve_name("u8")?;
	for b in bytes {
		cx.encode_number(u8id, i128::from(*b), out)?;
	}
	Ok(())
}

pub fn bytes_of(c: &Construction) -> Result<&[u8]> {
	match c {
		Construction::Literal(Value::Bytes(b)) => Ok(b),
		Construction::Literal(Value::String(s)) => Ok(s.as_bytes()),
		c => bail!("expected bytes, got {c:?}"),
	}
}

pub fn native_variant(native: &mir::NativeType) -> &str {
	native
		.specializer
		.as_ref()
		.map_or("default", |(_, variant)| variant.as_str())
}

pub fn variant_name<'v>(v: &'v Val) -> &'v str {
	native_variant(v.native)
}

pub fn decode_items(cx: &Encoding, t: &Target, n: usize, input: &mut Input) -> Result<Vec<Expr>> {
	let value = use_of(t.native, VALUE)?;
	let scope = t.scope();
	(0..n)
		.map(|i| {
			cx.decode_use(value, &scope, input)
				.with_context(|| format!("item {i}"))
		})
		.collect()
}

pub fn decode_bytes(cx: &Encoding, n: usize, input: &mut Input) -> Result<Vec<u8>> {
	let u8id = cx.resolve_name("u8")?;
	(0..n)
		.map(|i| {
			let b = cx.decode(u8id, &Args::new(), input)?;
			int_of(&b)
				.and_then(|n| u8::try_from(n).ok())
				.ok_or_else(|| anyhow!("byte {i} is {}, not a u8", describe(&b)))
		})
		.collect()
}

pub fn decode_count(cx: &Encoding, t: &Target, input: &mut Input) -> Result<usize> {
	if native_variant(t.native) == "inline" {
		let count = use_of(t.native, "Count")?;
		let n = cx.decode_use(count, &t.scope(), input).context("count")?;
		cx.count(&n).context("count")
	} else {
		let n = t
			.args
			.get(&Ident::new("count"))
			.ok_or_else(|| anyhow!("{} has no count argument", cx.name(t.id)))?;
		cx.count(n).context("count")
	}
}

pub fn record_fields(
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

pub fn construct_bytes_rust(c: &Construction) -> Result<rust::Tokens> {
	let bytes = bytes_of(c)?;
	Ok(quote!(vec![$(for b in bytes join(, ) => $(format!("{b}_u8")))]))
}

pub fn construct_bytes_js(c: &Construction) -> Result<js::Tokens> {
	let bytes = bytes_of(c)?.iter().map(u8::to_string).collect::<Vec<_>>().join(", ");
	Ok(quote!(new Uint8Array([$bytes])))
}

pub fn string_of(c: &Construction) -> Result<&str> {
	match c {
		Construction::Literal(Value::String(s)) => Ok(s),
		c => bail!("expected a string, got {c:?}"),
	}
}
