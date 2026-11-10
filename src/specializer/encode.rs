use std::collections::BTreeMap;

use anyhow::{anyhow, bail, ensure, Context as _, Result};

use super::expr::{describe, value_eq, Elaborator};
use super::Scalar;
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
			args: self.args,
			vars: &[],
		}
	}
}

#[derive(Clone, Copy)]
pub struct Scope<'a> {
	pub value: Option<(TypeId, &'a Construction)>,
	pub args: &'a Args,
	pub vars: &'a [(Ident, Expr)],
}

pub trait Encoder {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()>;
	fn field(&self, cx: &Encoding, v: &Val, name: &Ident) -> Result<Expr> {
		match v.c {
			Construction::Record(fields) => fields
				.iter()
				.find(|(n, _)| n == name)
				.map(|(_, e)| e.clone())
				.ok_or_else(|| anyhow!("{} has no field {name:?}", cx.name(v.id))),
			_ => bail!("{} has no field {name:?}", cx.name(v.id)),
		}
	}
	fn virtual_field(&self, cx: &Encoding, v: &Val, name: &Ident) -> Result<Expr> {
		bail!("{} has no virtual field {name:?}", cx.name(v.id))
	}
	fn cast(&self, cx: &Encoding, v: &Val, to: TypeId) -> Result<Expr> {
		bail!("{} can't be cast to {}", cx.name(v.id), cx.name(to))
	}
}

pub struct Encoding<'a> {
	pub elab: &'a Elaborator<'a>,
}

pub fn write_varint(out: &mut Vec<u8>, mut v: u64) {
	while v >= 0x80 {
		out.push((v as u8) | 0x80);
		v >>= 7;
	}
	out.push(v as u8);
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

fn float_of(e: &Expr) -> Option<f64> {
	match e {
		Expr::Construct(_, Construction::Literal(Value::Float(f))) | Expr::Const(Value::Float(f)) => {
			Some(*f)
		}
		e => int_of(e).map(|n| n as f64),
	}
}

fn width(s: Scalar) -> Option<u32> {
	Some(match s {
		Scalar::I8 | Scalar::U8 | Scalar::Bool => 8,
		Scalar::I16 | Scalar::U16 => 16,
		Scalar::I32 | Scalar::U32 | Scalar::F32 => 32,
		Scalar::I64 | Scalar::U64 | Scalar::F64 => 64,
		Scalar::String | Scalar::Bytes => return None,
	})
}

fn signed(s: Scalar) -> bool {
	matches!(s, Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64)
}

pub fn wrap(n: i128, s: Scalar) -> i128 {
	let Some(bits) = width(s) else { return n };
	let m = 1i128 << bits;
	let u = n.rem_euclid(m);
	if signed(s) && u >= m / 2 {
		u - m
	} else {
		u
	}
}

fn bits_of(n: i128, bits: u32) -> u64 {
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
					&& f.iter().all(|(n, a)| g.iter().any(|(m, b)| n == m && const_eq(a, b)))
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

impl<'a> Encoding<'a> {
	pub fn types(&self) -> &'a TypeMap {
		self.elab.types
	}

	pub fn name(&self, id: TypeId) -> String {
		self.elab.name(id)
	}

	pub fn resolve_name(&self, name: &str) -> Result<TypeId> {
		self.elab.resolve_name(name)
	}

	pub fn encode_number(&self, ty: TypeId, n: i128, out: &mut Vec<u8>) -> Result<()> {
		self.encode(&self.number(ty, n)?, &Args::new(), out)
	}

	pub fn number(&self, ty: TypeId, n: i128) -> Result<Expr> {
		self.elab.build(ty, &Expr::Const(Value::Signed(n)))
	}

	pub fn typed(&self, e: Expr, ty: TypeId) -> Result<Expr> {
		if e.is_untyped_literal() {
			self.elab.build(ty, &e)
		} else {
			Ok(e)
		}
	}

	fn encoder(&self, id: TypeId) -> Result<(&'a mir::NativeType, &'a dyn Encoder)> {
		let TypeKind::Native(n) = &self.types()[id].kind else {
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
		let encoder = self
			.elab
			.reg
			.encoder(name.as_str())
			.ok_or_else(|| anyhow!("no encoder {name:?} is registered, {} uses it", self.name(id)))?;
		Ok((n, encoder))
	}

	fn with_val<T>(
		&self,
		e: &Expr,
		args: &Args,
		f: impl FnOnce(&'a dyn Encoder, &Val) -> Result<T>,
	) -> Result<T> {
		let Expr::Construct(id, c) = e else {
			bail!("{} is not a constant value", describe(e));
		};
		let (native, encoder) = self.encoder(*id)?;
		f(
			encoder,
			&Val {
				id: *id,
				native,
				c,
				args,
			},
		)
	}

	pub fn encode(&self, e: &Expr, args: &Args, out: &mut Vec<u8>) -> Result<()> {
		if let Expr::Construct(id, _) = e {
			if self.types().is_void(*id) {
				return Ok(());
			}
		}
		self.with_val(e, args, |enc, v| enc.encode(self, v, out))
	}

	pub fn encode_root(&self, e: &Expr) -> Result<Vec<u8>> {
		let mut out = Vec::new();
		self.encode(e, &Args::new(), &mut out)?;
		Ok(out)
	}

	pub fn use_args(&self, u: &mir::Type, scope: &Scope) -> Result<Args> {
		let mut args = self.eval_args(&u.args, scope)?;
		let mut target = u.target;
		while let TypeKind::Alias(t) = &self.types()[target].kind {
			let inner = Scope {
				value: None,
				args: &args,
				vars: &[],
			};
			args = self.eval_args(&t.args, &inner)?;
			target = t.target;
		}
		Ok(args)
	}

	pub fn encode_use(
		&self,
		u: &mir::Type,
		value: &Expr,
		scope: &Scope,
		out: &mut Vec<u8>,
	) -> Result<()> {
		let value = self.typed(value.clone(), u.target)?;
		let args = self.use_args(u, scope)?;
		self.encode(&value, &args, out)
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

	pub fn field(&self, id: TypeId, c: &Construction, args: &Args, name: &Ident) -> Result<Expr> {
		let (native, encoder) = self.encoder(id)?;
		encoder.field(
			self,
			&Val {
				id,
				native,
				c,
				args,
			},
			name,
		)
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
					let (id, c, root) = base(&cur)?;
					let args = if root { scope.args } else { &empty };
					self.field(id, &c, args, to)?
				}
				FieldRefSegment::Virtual(name) => {
					let (id, c, root) = base(&cur)?;
					let args = if root { scope.args } else { &empty };
					let (native, encoder) = self.encoder(id)?;
					encoder.virtual_field(
						self,
						&Val {
							id,
							native,
							c: &c,
							args,
						},
						name,
					)?
				}
			};
			cur = Some(next);
		}
		cur.ok_or_else(|| anyhow!("empty field reference"))
	}

	fn arith(&self, op: crate::ast::BinOp, a: Expr, b: Expr) -> Result<Expr> {
		let ty = a.ty().or_else(|| b.ty());
		let scalar = ty.and_then(|t| scalar_of(self.types(), self.elab.resolve(t)));
		if matches!(scalar, Some(Scalar::F32 | Scalar::F64)) {
			let (x, y) = (
				float_of(&a).ok_or_else(|| anyhow!("{} is not a number", describe(&a)))?,
				float_of(&b).ok_or_else(|| anyhow!("{} is not a number", describe(&b)))?,
			);
			let r = match op {
				crate::ast::BinOp::Add => x + y,
				crate::ast::BinOp::Sub => x - y,
				crate::ast::BinOp::Mul => x * y,
			};
			return self.elab.build(ty.expect("float scalar"), &Expr::Const(Value::Float(r)));
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

	fn cast(&self, v: Expr, to: TypeId) -> Result<Expr> {
		let Expr::Construct(from, c) = &v else {
			return self.elab.build(to, &v);
		};
		let (from, to_r) = (self.elab.resolve(*from), self.elab.resolve(to));
		if from == to_r {
			return Ok(v);
		}
		let number = |id| scalar_of(self.types(), id).filter(|s| s.is_number());
		if let (Some(_), Some(t)) = (number(from), number(to_r)) {
			if matches!(t, Scalar::F32 | Scalar::F64) {
				let f = float_of(&v).ok_or_else(|| anyhow!("{} is not a number", describe(&v)))?;
				return self.elab.build(to, &Expr::Const(Value::Float(f)));
			}
			let n = match float_of(&v) {
				Some(f) if int_of(&v).is_none() => f.trunc() as i128,
				_ => int_of(&v).ok_or_else(|| anyhow!("{} is not a number", describe(&v)))?,
			};
			return self.number(to, wrap(n, t));
		}
		let args = Args::new();
		let (native, encoder) = self.encoder(from)?;
		encoder.cast(
			self,
			&Val {
				id: from,
				native,
				c,
				args: &args,
			},
			to,
		)
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
			Expr::Binary(op, a, b) => self.arith(*op, self.eval(a, scope)?, self.eval(b, scope)?)?,
			Expr::Cast(e, to) => self.cast(self.eval(e, scope)?, *to)?,
			Expr::Eq(a, b) => {
				let (a, b) = (self.eval(a, scope)?, self.eval(b, scope)?);
				Expr::Const(Value::Bool(const_eq(&a, &b)))
			}
			Expr::Record(_) | Expr::List(_) | Expr::Serialized(_) => {
				bail!("{} can't be evaluated here", describe(e))
			}
		})
	}

	pub fn truthy(&self, e: &Expr, scope: &Scope) -> Result<bool> {
		match self.eval(e, scope)? {
			Expr::Const(Value::Bool(b)) | Expr::Construct(_, Construction::Literal(Value::Bool(b))) => {
				Ok(b)
			}
			e => bail!("{} is not a condition", describe(&e)),
		}
	}
}

pub struct ScalarEncoding(pub &'static str);

impl Encoder for ScalarEncoding {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let Construction::Literal(value) = v.c else {
			bail!("{} is not a literal", cx.name(v.id));
		};
		let s = scalar_of(cx.types(), v.id).ok_or_else(|| {
			anyhow!("{} needs `@scalar` for `@encoding {}`", cx.name(v.id), self.0)
		})?;
		let e = Expr::Construct(v.id, v.c.clone());
		let int = || int_of(&e).ok_or_else(|| anyhow!("{value:?} is not an integer"));
		match (self.0, s) {
			("raw", Scalar::String | Scalar::Bytes) => match value {
				Value::String(s) => out.extend_from_slice(s.as_bytes()),
				Value::Bytes(b) => out.extend_from_slice(b),
				v => bail!("{v:?} is not a string or bytes"),
			},
			("le" | "be", s) => {
				let bits = width(s).ok_or_else(|| anyhow!("{s:?} has no fixed width"))?;
				let raw = match s {
					Scalar::F32 => {
						(float_of(&e).ok_or_else(|| anyhow!("{value:?} is not a number"))? as f32)
							.to_bits() as u64
					}
					Scalar::F64 => float_of(&e)
						.ok_or_else(|| anyhow!("{value:?} is not a number"))?
						.to_bits(),
					_ => bits_of(int()?, bits),
				};
				let bytes = raw.to_le_bytes();
				let bytes = &bytes[..(bits / 8) as usize];
				if self.0 == "le" {
					out.extend_from_slice(bytes);
				} else {
					out.extend(bytes.iter().rev());
				}
			}
			("varint" | "varint64" | "zigzag", s) if s.is_number() || s == Scalar::Bool => {
				ensure!(
					!matches!(s, Scalar::F32 | Scalar::F64),
					"{} can't be a varint",
					cx.name(v.id)
				);
				let bits = width(s).expect("integer scalar");
				let n = int()?;
				let u = match self.0 {
					"varint" => bits_of(n, bits),
					"varint64" => bits_of(n, 64),
					_ => bits_of((n << 1) ^ (n >> (bits - 1)), bits),
				};
				write_varint(out, u);
			}
			(e, s) => bail!("`@encoding {e}` can't encode {s:?} ({})", cx.name(v.id)),
		}
		Ok(())
	}
}

pub const SCALAR_ENCODINGS: &[&str] = &["varint", "varint64", "zigzag", "le", "be", "raw"];
