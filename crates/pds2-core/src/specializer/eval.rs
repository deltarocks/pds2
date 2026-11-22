use std::collections::BTreeMap;

use anyhow::{Context as _, Result, anyhow, bail};

use super::Scalar;
use super::expr::{Elaborator, ValueModel, describe, value_eq};
use crate::ast::{FieldRefSegment, Ident};
use crate::generator::common::scalar_of;
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, Field, TypeKind, TypeMap, Value};

pub type Args = BTreeMap<Ident, Expr>;

pub struct Val<'a> {
	pub id: TypeId,
	pub native: &'a mir::NativeType,
	pub c: &'a Construction,
	pub args: &'a Args,
}

impl Val<'_> {
	pub fn scope(&self) -> Scope<'_> {
		Scope {
			value: Some((self.id, self.c)),
			..Scope::of(self.args)
		}
	}
}

pub struct Target<'a> {
	pub id: TypeId,
	pub native: &'a mir::NativeType,
	pub args: &'a Args,
}

impl Target<'_> {
	pub fn scope(&self) -> Scope<'_> {
		Scope::of(self.args)
	}
}

#[derive(Clone, Copy)]
pub struct Scope<'a> {
	pub value: Option<(TypeId, &'a Construction)>,
	pub args: &'a Args,
	pub vars: &'a [(Ident, Expr)],
	pub fields: &'a [(Ident, Expr)],
}

impl<'a> Scope<'a> {
	pub fn of(args: &'a Args) -> Self {
		Scope {
			value: None,
			args,
			vars: &[],
			fields: &[],
		}
	}
}

pub fn int_of(e: &Expr) -> Option<i128> {
	match e {
		Expr::Construct(_, Construction::Literal(v)) | Expr::Const(v) => match v {
			Value::Signed(n) => Some(*n),
			Value::Unsigned(n) => i128::try_from(*n).ok(),
			Value::Bool(b) => Some(*b as i128),
			_ => None,
		},
		_ => None,
	}
}

pub fn float_of(e: &Expr) -> Option<f64> {
	match e {
		Expr::Construct(_, Construction::Literal(Value::Float(f)))
		| Expr::Const(Value::Float(f)) => Some(*f),
		e => int_of(e).map(|n| n as f64),
	}
}

pub fn width(s: Scalar) -> Option<u32> {
	Some(match s {
		Scalar::I8 | Scalar::U8 | Scalar::Bool => 8,
		Scalar::I16 | Scalar::U16 => 16,
		Scalar::I32 | Scalar::U32 | Scalar::F32 => 32,
		Scalar::I64 | Scalar::U64 | Scalar::F64 => 64,
		Scalar::String | Scalar::Bytes => return None,
	})
}

pub fn signed(s: Scalar) -> bool {
	matches!(s, Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64)
}

pub fn wrap(n: i128, s: Scalar) -> i128 {
	let Some(bits) = width(s) else { return n };
	let m = 1i128 << bits;
	let u = n.rem_euclid(m);
	if signed(s) && u >= m / 2 { u - m } else { u }
}

pub fn bits_of(n: i128, bits: u32) -> u64 {
	((n as u128) & ((1u128 << bits) - 1)) as u64
}

pub fn const_eq(a: &Expr, b: &Expr) -> bool {
	match (a, b) {
		(Expr::Construct(_, x), Expr::Construct(_, y)) => match (x, y) {
			(Construction::Literal(x), Construction::Literal(y)) => value_eq(x, y),
			(Construction::Variant(n, p), Construction::Variant(m, q)) => {
				n == m && p.len() == q.len() && p.iter().zip(q).all(|(a, b)| const_eq(a, b))
			}
			(Construction::Wrap(a), Construction::Wrap(b)) => const_eq(a, b),
			(Construction::Record(f), Construction::Record(g)) => {
				f.len() == g.len()
					&& f.iter()
						.all(|(n, a)| g.iter().any(|(m, b)| n == m && const_eq(a, b)))
			}
			(Construction::List(p), Construction::List(q)) => {
				p.len() == q.len() && p.iter().zip(q).all(|(a, b)| const_eq(a, b))
			}
			_ => false,
		},
		(Expr::List(p), Expr::List(q)) => {
			p.len() == q.len() && p.iter().zip(q).all(|(a, b)| const_eq(a, b))
		}
		(Expr::Const(x), Expr::Const(y))
		| (Expr::Const(x), Expr::Construct(_, Construction::Literal(y)))
		| (Expr::Construct(_, Construction::Literal(x)), Expr::Const(y)) => value_eq(x, y),
		_ => false,
	}
}

pub fn record_field(cx: &Elaborator, v: &Val, name: &Ident) -> Result<Expr> {
	match v.c {
		Construction::Record(fields) => fields
			.iter()
			.find(|(n, _)| n == name)
			.map(|(_, e)| e.clone())
			.ok_or_else(|| anyhow!("{} has no field {name:?}", cx.name(v.id))),
		_ => bail!("{} has no field {name:?}", cx.name(v.id)),
	}
}

impl<'a> Elaborator<'a> {
	pub fn types(&self) -> &'a TypeMap {
		self.types
	}

	pub fn encoder_name(&self, id: TypeId) -> Result<(&'a mir::NativeType, &'a Ident)> {
		let TypeKind::Native(n) = &self.types[id].kind else {
			bail!("{} is not a native", self.name(id));
		};
		let name = n
			.attr("encoding")
			.and_then(|a| a.first())
			.or(n.specializer.as_ref().map(|(s, _)| s))
			.ok_or_else(|| {
				anyhow!(
					"{} has no encoding, declare it with `@encoding <name>`",
					self.name(id)
				)
			})?;
		Ok((n, name))
	}

	pub fn number(&self, ty: TypeId, n: i128) -> Result<Expr> {
		self.build(ty, &Expr::Const(Value::Signed(n)))
	}

	pub fn count(&self, e: &Expr) -> Result<usize> {
		let n = int_of(e).ok_or_else(|| anyhow!("{} is not a number", describe(e)))?;
		usize::try_from(n).map_err(|_| anyhow!("count {n} is negative"))
	}

	fn value_of<'v>(
		&'v self,
		id: TypeId,
		c: &'v Construction,
		args: &'v Args,
	) -> Result<(Val<'v>, Option<&'v dyn ValueModel>)> {
		let (native, model) = self
			.native(id)
			.ok_or_else(|| anyhow!("{} is not a native", self.name(id)))?;
		Ok((
			Val {
				id,
				native,
				c,
				args,
			},
			model,
		))
	}

	pub fn field(&self, id: TypeId, c: &Construction, args: &Args, name: &Ident) -> Result<Expr> {
		match self.value_of(id, c, args)? {
			(v, Some(model)) => model.field(self, &v, name),
			(v, None) => record_field(self, &v, name),
		}
	}

	fn virtual_field(
		&self,
		id: TypeId,
		c: &Construction,
		args: &Args,
		name: &Ident,
	) -> Result<Expr> {
		match self.value_of(id, c, args)? {
			(v, Some(model)) => model.virtual_field(self, &v, name),
			_ => bail!("{} has no virtual field {name:?}", self.name(id)),
		}
	}

	pub fn use_args(&self, u: &mir::Type, scope: &Scope) -> Result<Args> {
		let mut args = self.eval_args(&u.args, scope)?;
		let mut target = u.target;
		while let TypeKind::Alias(t) = &self.types[target].kind {
			args = self.eval_args(&t.args, &Scope::of(&args))?;
			target = t.target;
		}
		Ok(args)
	}

	fn eval_args(&self, args: &[(Ident, Expr)], scope: &Scope) -> Result<Args> {
		args.iter()
			.map(|(n, e)| {
				let v = self
					.eval(e, scope)
					.with_context(|| format!("argument {n:?}"))?;
				Ok((n.clone(), v))
			})
			.collect()
	}

	fn path(&self, f: &Field, scope: &Scope) -> Result<Expr> {
		let empty = Args::new();
		let mut cur: Option<Expr> = None;
		for item in &f.0 {
			let base = |cur: &Option<Expr>| -> Result<(TypeId, Construction, bool)> {
				match cur {
					Some(Expr::Construct(id, c)) => Ok((*id, c.clone(), false)),
					Some(e) => bail!("{} has no fields", describe(e)),
					None => scope
						.value
						.map(|(id, c)| (id, c.clone(), true))
						.ok_or_else(|| anyhow!("{f:?} needs a value to read from")),
				}
			};
			let next = match &item.segment {
				FieldRefSegment::Arg(name) => scope
					.args
					.get(name)
					.cloned()
					.ok_or_else(|| anyhow!("argument {name:?} is not bound"))?,
				FieldRefSegment::Down { to, .. } => {
					match scope
						.fields
						.iter()
						.find(|(n, _)| n == to)
						.filter(|_| cur.is_none())
					{
						Some((_, e)) => e.clone(),
						None => {
							let (id, c, root) = base(&cur)?;
							let args = if root { scope.args } else { &empty };
							self.field(id, &c, args, to)?
						}
					}
				}
				FieldRefSegment::Virtual(name) => {
					let (id, c, root) = base(&cur)?;
					let args = if root { scope.args } else { &empty };
					self.virtual_field(id, &c, args, name)?
				}
			};
			cur = Some(next);
		}
		cur.ok_or_else(|| anyhow!("empty field reference"))
	}

	fn arith(&self, op: crate::ast::BinOp, a: Expr, b: Expr) -> Result<Expr> {
		let ty = a.ty().or_else(|| b.ty());
		let scalar = ty.and_then(|t| scalar_of(self.types, self.resolve(t)));
		if matches!(scalar, Some(Scalar::F32 | Scalar::F64)) {
			let (x, y) = (
				float_of(&a).ok_or_else(|| anyhow!("{} is not a number", describe(&a)))?,
				float_of(&b).ok_or_else(|| anyhow!("{} is not a number", describe(&b)))?,
			);
			let r = match op {
				crate::ast::BinOp::Add => x + y,
				crate::ast::BinOp::Sub => x - y,
				crate::ast::BinOp::Mul => x * y,
				crate::ast::BinOp::Div => x / y,
			};
			return self.build(ty.expect("float scalar"), &Expr::Const(Value::Float(r)));
		}
		let (x, y) = (
			int_of(&a).ok_or_else(|| anyhow!("{} is not a number", describe(&a)))?,
			int_of(&b).ok_or_else(|| anyhow!("{} is not a number", describe(&b)))?,
		);
		let r = op
			.eval(x, y)
			.ok_or_else(|| anyhow!("{x} {} {y} overflows", op.symbol()))?;
		match (ty, scalar) {
			(Some(ty), Some(s)) => self.number(ty, wrap(r, s)),
			_ => Ok(Expr::Const(Value::Signed(r))),
		}
	}

	fn cast_value(&self, v: Expr, to: TypeId) -> Result<Expr> {
		let Expr::Construct(from, c) = &v else {
			return self.build(to, &v);
		};
		let (from, to_r) = (self.resolve(*from), self.resolve(to));
		if from == to_r {
			return Ok(v);
		}
		let number = |id| scalar_of(self.types, id).filter(|s| s.is_number());
		if let (Some(_), Some(t)) = (number(from), number(to_r)) {
			if matches!(t, Scalar::F32 | Scalar::F64) {
				let f = float_of(&v).ok_or_else(|| anyhow!("{} is not a number", describe(&v)))?;
				return self.build(to, &Expr::Const(Value::Float(f)));
			}
			let n = match float_of(&v) {
				Some(f) if int_of(&v).is_none() => f.trunc() as i128,
				_ => int_of(&v).ok_or_else(|| anyhow!("{} is not a number", describe(&v)))?,
			};
			return self.number(to, wrap(n, t));
		}
		let args = Args::new();
		match self.value_of(from, c, &args)? {
			(v, Some(model)) => model.cast_value(self, &v, to),
			_ => bail!("{} can't be cast to {}", self.name(from), self.name(to)),
		}
	}

	pub fn eval(&self, e: &Expr, scope: &Scope) -> Result<Expr> {
		Ok(match e {
			Expr::Construct(..) | Expr::Const(_) => e.clone(),
			Expr::Var(n, _) => scope
				.vars
				.iter()
				.find(|(v, _)| v == n)
				.map(|(_, e)| e.clone())
				.ok_or_else(|| anyhow!("{n:?} is not bound"))?,
			Expr::Field(f) => self.path(f, scope)?,
			Expr::Binary(op, a, b) => {
				self.arith(*op, self.eval(a, scope)?, self.eval(b, scope)?)?
			}
			Expr::Cast(e, to) => self.cast_value(self.eval(e, scope)?, *to)?,
			Expr::Typed(e, ty) => self.coerce(self.eval(e, scope)?, *ty)?,
			Expr::Eq(a, b) => {
				let (a, b) = (self.eval(a, scope)?, self.eval(b, scope)?);
				Expr::Const(Value::Bool(const_eq(&a, &b)))
			}
			Expr::Record(_) | Expr::List(_) | Expr::Call(..) => {
				bail!("{} can't be evaluated here", describe(e))
			}
		})
	}

	pub fn truthy(&self, e: &Expr, scope: &Scope) -> Result<bool> {
		match self.eval(e, scope)? {
			Expr::Const(Value::Bool(b))
			| Expr::Construct(_, Construction::Literal(Value::Bool(b))) => Ok(b),
			e => bail!("{} is not a condition", describe(&e)),
		}
	}
}
