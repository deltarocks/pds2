use anyhow::{anyhow, Context as _, Result};
use pds2_core::ast::Ident;
use pds2_core::mir::Expr;
use pds2_core::specializer::eval::{int_of, Args, Target, Val};
use pds2_core::specializer::expr::describe;
use pds2_core::specializer::reflect::{native_variant, use_of};
use pds2_core::specializer::VALUE;

use crate::{Encoding, Input};

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
