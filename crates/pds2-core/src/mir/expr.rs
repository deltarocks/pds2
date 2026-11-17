use crate::{
	ast::{BinOp, Ident},
	hir::TypeId,
};

use super::{Field, Value};

#[derive(Debug, Clone)]
pub enum Expr {
	Var(Ident, TypeId),
	Field(Field),
	Const(Value),
	Record(Vec<(Ident, Expr)>),
	List(Vec<Expr>),
	Call(Ident, Vec<Expr>),
	Construct(TypeId, Construction),
	Cast(Box<Expr>, TypeId),
	Binary(BinOp, Box<Expr>, Box<Expr>),
	Eq(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum Construction {
	Literal(Value),
	Variant(Ident, Vec<Expr>),
	Wrap(Box<Expr>),
	Record(Vec<(Ident, Expr)>),
	List(Vec<Expr>),
}

impl Expr {
	pub fn ty(&self) -> Option<TypeId> {
		match self {
			Expr::Var(_, t) | Expr::Construct(t, _) | Expr::Cast(_, t) => Some(*t),
			Expr::Field(f) => f.0.last().map(|i| i.to.get()),
			Expr::Binary(_, a, b) => a.ty().or_else(|| b.ty()),
			Expr::Const(_) | Expr::Record(_) | Expr::List(_) | Expr::Call(..) | Expr::Eq(..) => None,
		}
	}

	pub fn is_untyped_literal(&self) -> bool {
		matches!(
			self,
			Expr::Const(_) | Expr::Record(_) | Expr::List(_) | Expr::Call(..)
		)
	}

	pub fn as_field(&self) -> Option<&Field> {
		match self {
			Expr::Field(f) => Some(f),
			_ => None,
		}
	}

	pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
		f(self);
		match self {
			Expr::Var(..) | Expr::Field(_) | Expr::Const(_) => {}
			Expr::Record(fields) | Expr::Construct(_, Construction::Record(fields)) => {
				for (_, e) in fields {
					e.walk(f);
				}
			}
			Expr::List(items)
			| Expr::Call(_, items)
			| Expr::Construct(_, Construction::List(items) | Construction::Variant(_, items)) => {
				for e in items {
					e.walk(f);
				}
			}
			Expr::Construct(_, Construction::Literal(_)) => {}
			Expr::Construct(_, Construction::Wrap(e)) | Expr::Cast(e, _) => e.walk(f),
			Expr::Binary(_, a, b) | Expr::Eq(a, b) => {
				a.walk(f);
				b.walk(f);
			}
		}
	}
}
