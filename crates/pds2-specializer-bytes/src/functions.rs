use anyhow::{bail, Context as _, Result};
use pds2_core::hir::TypeId;
use pds2_core::mir::{Construction, Expr, Value};
use pds2_core::specializer::expr::{describe, Elaborator};
use pds2_core::specializer::Function;

use crate::Encoding;

pub struct Serialized;

impl Function for Serialized {
	fn call(&self, cx: &Elaborator, _ty: Option<TypeId>, args: &[Expr]) -> Result<Option<Expr>> {
		let [value] = args else {
			bail!("`serialized(value)` takes one value");
		};
		let value = cx.elaborate(value.clone())?;
		let bytes = Encoding::new(cx)
			.encode_root(&value)
			.context("serialized(...)")?;
		Ok(Some(Expr::Const(Value::Bytes(bytes))))
	}
}

pub struct Deserialized;

impl Function for Deserialized {
	fn call(&self, cx: &Elaborator, ty: Option<TypeId>, args: &[Expr]) -> Result<Option<Expr>> {
		let Some(ty) = ty else {
			return Ok(None);
		};
		let [bytes] = args else {
			bail!("`deserialized(bytes)` takes one value");
		};
		let bytes = match bytes {
			Expr::Call(..) => cx.elaborate(bytes.clone())?,
			e => cx.operand(e.clone())?,
		};
		let bytes = match bytes {
			Expr::Const(Value::Bytes(b)) | Expr::Construct(_, Construction::Literal(Value::Bytes(b))) => b,
			Expr::Const(Value::String(s))
			| Expr::Construct(_, Construction::Literal(Value::String(s))) => s.into_bytes(),
			e => bail!("deserialized(...) takes bytes, got {}", describe(&e)),
		};
		let enc = Encoding::new(cx);
		let value = enc
			.decode_root(ty, &bytes)
			.with_context(|| format!("deserialized(...) as {}", cx.name(ty)))?;
		let skipped = enc.skipped();
		if !skipped.is_empty() {
			let list: Vec<String> = skipped.iter().map(|(what, n)| format!("{what} ({n}x)")).collect();
			tracing::warn!(
				"deserialized(...) as {} skipped {}",
				cx.name(ty),
				list.join(", ")
			);
		}
		Ok(Some(value))
	}
}
