use anyhow::{anyhow, bail, ensure, Context as _, Result};

use super::encode::Encoding;
use super::{Registry, Scalar};
use crate::ast::{self, BinOp, Ident};
use crate::generator::common::{sanitize, scalar_of};
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, TypeKind, TypeMap, Value};

pub trait ValueModel {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		_native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		bail!("{} can't be written as {}", cx.name(id), describe(value))
	}
	fn casts(&self, _cx: &Elaborator, _id: TypeId, _native: &mir::NativeType) -> Vec<TypeId> {
		vec![]
	}
	fn exprs(&self, _cx: &Elaborator, _id: TypeId, _native: &mir::NativeType) -> Vec<Expr> {
		vec![]
	}
	fn literal(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &Construction,
	) -> Result<Expr> {
		cx.construction_literal(c)
	}
}

pub fn source(e: &Expr) -> Result<ast::Value> {
	let name = |n: &str| ast::Value::Item(ast::Item::simple_builder(n).build());
	Ok(match e {
		Expr::Const(Value::Signed(n)) => ast::Value::Int(*n),
		Expr::Const(Value::Unsigned(n)) => {
			ast::Value::Int(i128::try_from(*n).map_err(|_| anyhow!("{n} is too large to write"))?)
		}
		Expr::Const(Value::Float(f)) => {
			ensure!(f.is_finite(), "{f} can't be written");
			ast::Value::Float(ast::Float(*f))
		}
		Expr::Const(Value::Bool(b)) => name(if *b { "true" } else { "false" }),
		Expr::Const(Value::String(s)) => ast::Value::String {
			string: s.clone(),
			is_block: false,
		},
		Expr::Const(Value::Bytes(b)) => ast::Value::Bytes(b.clone()),
		Expr::Const(Value::UnionDescriminator(n)) => name(n.as_str()),
		Expr::Record(fields) => ast::Value::Record(
			fields
				.iter()
				.map(|(n, e)| Ok((n.clone(), source(e)?)))
				.collect::<Result<_>>()?,
		),
		Expr::List(items) => ast::Value::List(items.iter().map(source).collect::<Result<_>>()?),
		e => bail!("{} is not a literal", describe(e)),
	})
}

impl Scalar {
	pub fn accepts(&self, value: &Value) -> bool {
		match (self, value) {
			(Scalar::Bool, v) => matches!(v, Value::Bool(_)),
			(Scalar::String, v) => matches!(v, Value::String(_)),
			(Scalar::Bytes, v) => matches!(v, Value::Bytes(_) | Value::String(_)),
			(Scalar::F32 | Scalar::F64, v) => {
				matches!(v, Value::Signed(_) | Value::Unsigned(_) | Value::Float(_))
			}
			(s, Value::Signed(n)) => s.range().contains(n),
			(s, Value::Unsigned(n)) => i128::try_from(*n).is_ok_and(|n| s.range().contains(&n)),
			_ => false,
		}
	}

	fn range(self) -> std::ops::RangeInclusive<i128> {
		match self {
			Scalar::I8 => i8::MIN.into()..=i8::MAX.into(),
			Scalar::U8 => 0..=u8::MAX.into(),
			Scalar::I16 => i16::MIN.into()..=i16::MAX.into(),
			Scalar::U16 => 0..=u16::MAX.into(),
			Scalar::I32 => i32::MIN.into()..=i32::MAX.into(),
			Scalar::U32 => 0..=u32::MAX.into(),
			Scalar::I64 => i64::MIN.into()..=i64::MAX.into(),
			Scalar::U64 => 0..=u64::MAX.into(),
			_ => i128::MIN..=i128::MAX,
		}
	}
}

pub fn describe(e: &Expr) -> String {
	match e {
		Expr::Const(v) | Expr::Construct(_, Construction::Literal(v)) => format!("{v:?}"),
		Expr::Record(_) => "a record".to_owned(),
		Expr::List(_) => "a list".to_owned(),
		e => format!("{e:?}"),
	}
}

pub fn value_eq(a: &Value, b: &Value) -> bool {
	let int = |v: &Value| match v {
		Value::Signed(n) => Some(*n),
		Value::Unsigned(n) => i128::try_from(*n).ok(),
		_ => None,
	};
	match (a, b) {
		(Value::Bool(a), Value::Bool(b)) => a == b,
		(Value::String(a), Value::String(b)) => a == b,
		(Value::UnionDescriminator(a), Value::UnionDescriminator(b)) => {
			sanitize(a.as_str()) == sanitize(b.as_str())
		}
		_ => matches!((int(a), int(b)), (Some(a), Some(b)) if a == b),
	}
}

pub struct Elaborator<'a> {
	pub types: &'a TypeMap,
	pub reg: &'a Registry,
}

impl Elaborator<'_> {
	pub fn resolve(&self, mut id: TypeId) -> TypeId {
		while let TypeKind::Alias(t) = &self.types[id].kind {
			id = t.target;
		}
		id
	}

	fn native(&self, id: TypeId) -> Option<(&mir::NativeType, Option<&dyn ValueModel>)> {
		let TypeKind::Native(n) = &self.types[id].kind else {
			return None;
		};
		let model = n
			.specializer
			.as_ref()
			.and_then(|(s, _)| self.reg.value_model(s.as_str()));
		Some((n, model))
	}

	pub fn build(&self, ty: TypeId, value: &Expr) -> Result<Expr> {
		if let Expr::Deserialized(bytes) = value {
			return self.deserialize(ty, bytes);
		}
		let id = self.resolve(ty);
		let construction = if self.types.is_void(id) {
			ensure!(
				matches!(value, Expr::Record(f) if f.is_empty()),
				"{} carries no data, write it as `()`, got {}",
				self.name(id),
				describe(value)
			);
			Construction::Record(vec![])
		} else if let Some(s) = scalar_of(self.types, id) {
			match value {
				Expr::Const(v) if s.accepts(v) => Construction::Literal(v.clone()),
				v => bail!("{} can't be written as {}", self.name(id), describe(v)),
			}
		} else {
			let (native, model) = self
				.native(id)
				.ok_or_else(|| anyhow!("{} can't be written as a value", self.name(id)))?;
			let model =
				model.ok_or_else(|| anyhow!("{} can't be written as a value", self.name(id)))?;
			model.construct(self, id, native, value)?
		};
		Ok(Expr::Construct(id, construction))
	}

	fn deserialize(&self, ty: TypeId, bytes: &Expr) -> Result<Expr> {
		let bytes = match self.operand(bytes.clone())? {
			Expr::Const(Value::Bytes(b)) | Expr::Construct(_, Construction::Literal(Value::Bytes(b))) => b,
			Expr::Const(Value::String(s))
			| Expr::Construct(_, Construction::Literal(Value::String(s))) => s.into_bytes(),
			e => bail!("deserialized(...) takes bytes, got {}", describe(&e)),
		};
		let cx = Encoding::new(self);
		let value = cx
			.decode_root(ty, &bytes)
			.with_context(|| format!("deserialized(...) as {}", self.name(ty)))?;
		let skipped = cx.skipped();
		if !skipped.is_empty() {
			let list: Vec<String> = skipped.iter().map(|(what, n)| format!("{what} ({n}x)")).collect();
			tracing::warn!(
				"deserialized(...) as {} skipped {}",
				self.name(ty),
				list.join(", ")
			);
		}
		Ok(value)
	}

	pub fn literal(&self, e: &Expr) -> Result<Expr> {
		Ok(match e {
			Expr::Const(_) => e.clone(),
			Expr::Record(fields) => self.record_literal(fields)?,
			Expr::List(items) => self.list_literal(items)?,
			Expr::Construct(id, _) if self.types.is_void(*id) => Expr::Record(vec![]),
			Expr::Construct(id, c) => match (scalar_of(self.types, *id), c) {
				(Some(Scalar::F32), Construction::Literal(Value::Float(f))) => {
					let short = (*f as f32).to_string().parse().unwrap_or(*f);
					Expr::Const(Value::Float(short))
				}
				(Some(_), Construction::Literal(v)) => Expr::Const(v.clone()),
				_ => {
					let (native, model) = self
						.native(*id)
						.ok_or_else(|| anyhow!("{} has no literal form", self.name(*id)))?;
					let model =
						model.ok_or_else(|| anyhow!("{} has no literal form", self.name(*id)))?;
					model.literal(self, *id, native, c)?
				}
			},
			e => bail!("{} is not a constant value", describe(e)),
		})
	}

	pub fn record_literal(&self, fields: &[(Ident, Expr)]) -> Result<Expr> {
		Ok(Expr::Record(
			fields
				.iter()
				.map(|(n, e)| Ok((n.clone(), self.literal(e).with_context(|| format!("field {n:?}"))?)))
				.collect::<Result<_>>()?,
		))
	}

	pub fn list_literal(&self, items: &[Expr]) -> Result<Expr> {
		Ok(Expr::List(
			items
				.iter()
				.enumerate()
				.map(|(i, e)| self.literal(e).with_context(|| format!("item {i}")))
				.collect::<Result<_>>()?,
		))
	}

	pub fn construction_literal(&self, c: &Construction) -> Result<Expr> {
		Ok(match c {
			Construction::Literal(v) => Expr::Const(v.clone()),
			Construction::Wrap(e) => self.literal(e)?,
			Construction::Record(fields) => self.record_literal(fields)?,
			Construction::List(items) => self.list_literal(items)?,
			Construction::Variant(n, p) => match p.as_slice() {
				[] => Expr::Const(Value::UnionDescriminator(n.clone())),
				[v] => Expr::Record(vec![(n.clone(), self.literal(v)?)]),
				_ => bail!("variant {n:?} holding {} values has no literal form", p.len()),
			},
		})
	}

	pub fn construct(&self, ty: TypeId, value: &Value) -> Option<Expr> {
		self.build(ty, &Expr::Const(value.clone())).ok()
	}

	pub fn casts(&self, ty: TypeId) -> Vec<TypeId> {
		let id = self.resolve(ty);
		match self.native(id) {
			Some((native, Some(model))) => model.casts(self, id, native),
			_ => vec![],
		}
	}

	pub fn exprs(&self, id: TypeId) -> Result<Vec<Expr>> {
		let Some((native, Some(model))) = self.native(id) else {
			return Ok(vec![]);
		};
		model
			.exprs(self, id, native)
			.into_iter()
			.map(|e| self.elaborate(e))
			.collect()
	}

	pub fn name(&self, id: TypeId) -> String {
		self.types[id].name.readable_name()
	}

	pub fn resolve_name(&self, name: &str) -> Result<TypeId> {
		self.types
			.0
			.iter()
			.find(|(_, d)| d.name.readable_name() == name)
			.map(|(id, _)| *id)
			.ok_or_else(|| anyhow!("the `{name}` native is not declared"))
	}

	fn compare(&self, subject: Expr, value: &Value) -> Result<Expr> {
		let ty = subject
			.ty()
			.ok_or_else(|| anyhow!("{value:?} compared with an untyped expression"))?;
		if let Some(c) = self.construct(ty, value) {
			return Ok(Expr::Eq(Box::new(subject), Box::new(c)));
		}
		for to in self.casts(ty) {
			if let Ok(e) = self.compare(Expr::Cast(Box::new(subject.clone()), to), value) {
				return Ok(e);
			}
		}
		bail!("{value:?} can't be compared with {}", self.name(ty))
	}

	fn numeric(&self, ty: TypeId) -> Option<Scalar> {
		scalar_of(self.types, ty).filter(|s| s.is_number())
	}

	fn operand(&self, e: Expr) -> Result<Expr> {
		if e.is_untyped_literal() {
			Ok(e)
		} else {
			self.elaborate(e)
		}
	}

	fn typed(&self, e: Expr, ty: TypeId) -> Result<Expr> {
		if e.is_untyped_literal() {
			self.build(ty, &e)
		} else {
			Ok(e)
		}
	}

	pub fn coerce(&self, e: Expr, ty: TypeId) -> Result<Expr> {
		let e = self.typed(self.operand(e)?, ty)?;
		let got = e
			.ty()
			.ok_or_else(|| anyhow!("expected {}, got an untyped value", self.name(ty)))?;
		ensure!(
			self.resolve(got) == self.resolve(ty),
			"expected {}, got {}; cast it explicitly",
			self.name(ty),
			self.name(got)
		);
		Ok(e)
	}

	fn binary(&self, op: BinOp, a: Expr, b: Expr) -> Result<Expr> {
		let (a, b) = (self.operand(a)?, self.operand(b)?);
		if let (Expr::Const(x), Expr::Const(y)) = (&a, &b) {
			let int = |v: &Value| match v {
				Value::Signed(n) => Some(*n),
				Value::Unsigned(n) => i128::try_from(*n).ok(),
				_ => None,
			};
			let (Some(x), Some(y)) = (int(x), int(y)) else {
				bail!("{} needs numbers, got {x:?} and {y:?}", op.symbol());
			};
			let n = op
				.eval(x, y)
				.ok_or_else(|| anyhow!("{x} {} {y} overflows", op.symbol()))?;
			return Ok(Expr::Const(Value::Signed(n)));
		}
		let ty = a
			.ty()
			.or_else(|| b.ty())
			.ok_or_else(|| anyhow!("{} of untyped operands", op.symbol()))?;
		let (a, b) = (self.typed(a, ty)?, self.typed(b, ty)?);
		let (ta, tb) = (a.ty().map(|t| self.resolve(t)), b.ty().map(|t| self.resolve(t)));
		ensure!(
			ta == tb,
			"{} of different types {:?} and {:?}, cast one explicitly",
			op.symbol(),
			ta.map(|t| self.name(t)),
			tb.map(|t| self.name(t))
		);
		ensure!(
			self.numeric(ty).is_some(),
			"{} needs numbers, got {}",
			op.symbol(),
			self.name(ty)
		);
		Ok(Expr::Binary(op, Box::new(a), Box::new(b)))
	}

	fn cast(&self, e: Expr, to: TypeId) -> Result<Expr> {
		let e = self.operand(e)?;
		if e.is_untyped_literal() {
			return self.typed(e, to);
		}
		let from = e
			.ty()
			.ok_or_else(|| anyhow!("cast of an untyped expression"))?;
		if self.resolve(from) == self.resolve(to) {
			return Ok(e);
		}
		let numeric = self.numeric(from).is_some() && self.numeric(to).is_some();
		ensure!(
			numeric || self.casts(from).contains(&to),
			"{} can't be cast to {}",
			self.name(from),
			self.name(to)
		);
		Ok(Expr::Cast(Box::new(e), to))
	}

	pub fn elaborate(&self, expr: Expr) -> Result<Expr> {
		Ok(match expr {
			Expr::Binary(op, a, b) => self.binary(op, *a, *b)?,
			Expr::Eq(a, b) => match (*a, *b) {
				(Expr::Const(a), Expr::Const(b)) => bail!("{a:?} compared with {b:?}"),
				(x, Expr::Const(v)) | (Expr::Const(v), x) => self.compare(self.elaborate(x)?, &v)?,
				(a, b) => {
					let (a, b) = (self.elaborate(a)?, self.elaborate(b)?);
					let (ta, tb) = (a.ty().map(|t| self.resolve(t)), b.ty().map(|t| self.resolve(t)));
					ensure!(
						ta.is_some() && ta == tb,
						"comparison of different types {:?} and {:?}",
						ta.map(|t| self.name(t)),
						tb.map(|t| self.name(t))
					);
					Expr::Eq(Box::new(a), Box::new(b))
				}
			},
			Expr::Cast(e, to) => self.cast(*e, to)?,
			Expr::Serialized(e) => {
				let e = self.elaborate(*e)?;
				let bytes = Encoding::new(self)
					.encode_root(&e)
					.context("serialized(...)")?;
				Expr::Const(Value::Bytes(bytes))
			}
			e if e.is_untyped_literal() => {
				bail!("{} needs a type, write it as `Type(...)`", describe(&e))
			}
			e => e,
		})
	}
}
