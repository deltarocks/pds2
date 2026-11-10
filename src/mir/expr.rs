use crate::{ast::Ident, hir::TypeId};

use super::{Field, Value};

#[derive(Debug, Clone)]
pub enum Expr {
	Var(Ident, TypeId),
	Field(Field),
	Const(Value),
	Construct(TypeId, Construction),
	Cast(Box<Expr>, TypeId),
	Eq(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum Construction {
	Literal(Value),
	Variant(Ident, Vec<Expr>),
	Wrap(Box<Expr>),
}

impl Expr {
	pub fn ty(&self) -> Option<TypeId> {
		match self {
			Expr::Var(_, t) | Expr::Construct(t, _) | Expr::Cast(_, t) => Some(*t),
			Expr::Field(f) => f.0.last().map(|i| i.to.get()),
			Expr::Const(_) | Expr::Eq(..) => None,
		}
	}

	pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
		f(self);
		match self {
			Expr::Var(..) | Expr::Field(_) | Expr::Const(_) => {}
			Expr::Construct(_, c) => match c {
				Construction::Literal(_) => {}
				Construction::Variant(_, args) => {
					for a in args {
						a.walk(f);
					}
				}
				Construction::Wrap(e) => e.walk(f),
			},
			Expr::Cast(e, _) => e.walk(f),
			Expr::Eq(a, b) => {
				a.walk(f);
				b.walk(f);
			}
		}
	}
}
