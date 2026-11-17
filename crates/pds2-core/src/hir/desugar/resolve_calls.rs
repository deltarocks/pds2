use anyhow::Result;

use crate::hir::{Expr, Namespace, Reference, ReferenceKind, Traverse, TraverseSkip, TypeKind, Visitor};
use crate::specializer::Registry;

struct ResolveCalls;
impl Visitor for ResolveCalls {
	fn visit_expr(&mut self, e: &mut Expr, reg: &Registry) -> Result<()> {
		let Expr::Cast(operand, ty) = e else {
			return Ok(());
		};
		let TypeKind::Reference(Reference {
			ty: ReferenceKind::Path(path),
			args,
			types,
			body,
		}) = ty.as_ref()
		else {
			return Ok(());
		};
		let Some(name) = path.simple_name() else {
			return Ok(());
		};
		if !args.is_empty() || !types.is_empty() || !body.is_empty() || reg.function(name.as_str()).is_none() {
			return Ok(());
		}
		let operand = std::mem::replace(operand.as_mut(), Expr::List(vec![]));
		*e = Expr::Call(name, vec![operand]);
		Ok(())
	}
}

pub fn resolve_calls(ns: &mut Namespace, reg: &Registry) -> Result<()> {
	ns.traverse(&mut ResolveCalls, TraverseSkip::empty(), reg)
}
