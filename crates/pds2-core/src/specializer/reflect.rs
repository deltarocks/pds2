use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use super::eval::{int_of, Val};
use super::expr::{describe, Elaborator};
use super::{specialization_use, LowerCx};
use crate::ast::Ident;
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
		parts.push(quote!($(cx.names.field(id, n.as_str())): $((cx.lower)(e)?)));
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
