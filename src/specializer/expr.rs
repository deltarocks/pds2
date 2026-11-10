use anyhow::{anyhow, bail, ensure, Result};

use super::{Registry, Scalar};
use crate::ast::BinOp;
use crate::generator::common::{sanitize, scalar_of};
use crate::hir::TypeId;
use crate::mir::{self, Construction, Expr, TypeKind, TypeMap, Value};

pub trait ValueModel {
	fn construct(
		&self,
		_cx: &Elaborator,
		_id: TypeId,
		_native: &mir::NativeType,
		_value: &Value,
	) -> Option<Construction> {
		None
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
			(_, v) => matches!(v, Value::Signed(_) | Value::Unsigned(_)),
		}
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

	pub fn construct(&self, ty: TypeId, value: &Value) -> Option<Expr> {
		let id = self.resolve(ty);
		let construction = match scalar_of(self.types, id) {
			Some(s) => s
				.accepts(value)
				.then(|| Construction::Literal(value.clone()))?,
			None => {
				let (native, model) = self.native(id)?;
				model?.construct(self, id, native, value)?
			}
		};
		Some(Expr::Construct(id, construction))
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

	fn name(&self, id: TypeId) -> String {
		self.types[id].name.readable_name()
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
		scalar_of(self.types, ty).filter(|s| *s != Scalar::Bool)
	}

	fn operand(&self, e: Expr) -> Result<Expr> {
		match e {
			Expr::Const(_) => Ok(e),
			e => self.elaborate(e),
		}
	}

	fn typed(&self, e: Expr, ty: TypeId) -> Result<Expr> {
		match e {
			Expr::Const(v) => self
				.construct(ty, &v)
				.ok_or_else(|| anyhow!("{v:?} can't be constructed as {}", self.name(ty))),
			e => Ok(e),
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
		if let Expr::Const(_) = e {
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
			Expr::Const(v) => bail!("constant {v:?} needs a type"),
			e => e,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::Elaborator;
	use crate::mir::{self, Construction, Expr, TypeKind, Value};
	use crate::specializer::{register_protodef_types, union, Registry};

	const SRC: &str = r#"
		@scalar u8 def_native(u8);
		def_native(void) { is_void; };
		@specializer container
		def_native(container) {
			body {
				item(field) { type(Value); };
				item(virtual_field) { argument(value); type(Value); };
			};
		};
		@specializer union
		def_native(union) {
			body {
				item(variant) { argument(match); type(Value) => void; };
				item(default) { anonymous; type(Value) => void; };
			};
			variant(external) { argument(tag); };
			variant(inline) { type(Tag); };
		};
		def(Kind) => union(Tag: u8) {
			variant(a, match: 1);
			variant(b, match: 2) => u8;
			default;
		};
		def(T) => container {
			field(k) => Kind;
			field(v) => union(tag: ../k) {
				variant(x, match: "a");
				variant(y, match: 2);
				variant(z, match: 7);
				variant(w, match: 1);
			};
		};
	"#;

	fn literal(e: &Expr) -> Option<i128> {
		match e {
			Expr::Construct(_, Construction::Literal(Value::Signed(n))) => Some(*n),
			_ => None,
		}
	}

	#[test]
	fn union_tags_compare_by_construction_or_cast() {
		let mut reg = Registry::default();
		register_protodef_types(&mut reg);
		let ast = crate::ast::parse(SRC).unwrap();
		let types = mir::from_hir(&crate::hir::parse_ast(ast, &reg).unwrap(), &reg).unwrap();
		let elab = Elaborator {
			types: &types,
			reg: &reg,
		};
		let (id, _) = types
			.0
			.iter()
			.find(|(_, d)| {
				matches!(&d.kind, TypeKind::Native(n) if union::view(n).is_some_and(|u| matches!(u.tag, union::TagView::External(_))))
			})
			.unwrap();
		let exprs = elab.exprs(*id).unwrap();
		let [x, y, z, w] = exprs.as_slice() else {
			panic!("{exprs:?}");
		};
		let eq = |e: &Expr| match e {
			Expr::Eq(a, b) => Some((a.as_ref().clone(), b.as_ref().clone())),
			_ => None,
		};

		let (lhs, rhs) = eq(x).unwrap();
		assert!(matches!(lhs, Expr::Var(..)), "{x:?}");
		assert!(
			matches!(&rhs, Expr::Construct(_, Construction::Variant(n, p)) if n.as_str() == "a" && p.is_empty()),
			"{x:?}"
		);

		let (lhs, rhs) = eq(y).unwrap();
		assert!(matches!(lhs, Expr::Cast(..)), "{y:?}");
		assert_eq!(literal(&rhs), Some(2), "{y:?}");

		let (_, rhs) = eq(z).unwrap();
		let Expr::Construct(_, Construction::Variant(n, payload)) = &rhs else {
			panic!("{z:?}");
		};
		assert_eq!(n.as_str(), "default");
		assert_eq!(payload.iter().map(literal).collect::<Vec<_>>(), [Some(7)]);

		let (lhs, rhs) = eq(w).unwrap();
		assert!(matches!(lhs, Expr::Var(..)), "{w:?}");
		assert!(
			matches!(&rhs, Expr::Construct(_, Construction::Variant(n, _)) if n.as_str() == "a"),
			"{w:?}"
		);
	}
}
