use anyhow::{anyhow, bail, ensure, Context as _, Result};

use super::{Registry, Scalar};
use crate::ast::BinOp;
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
		Expr::Const(v) => format!("{v:?}"),
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
				let bytes = super::encode::Encoding { elab: self }
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
