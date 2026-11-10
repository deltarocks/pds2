use std::collections::HashMap;

use anyhow::Result;

use crate::{
	ast::Ident,
	hir::{
		Expr, Namespace, NamespaceItem, Reference, ReferenceKind, Traverse, TraverseSkip, TypeId,
		TypeKind, Visitor,
	},
	specializer::Registry,
};

fn syntactic_type(e: &Expr) -> Option<TypeKind> {
	match e {
		Expr::Cast(_, t) => Some((**t).clone()),
		Expr::Binary(_, a, b) => syntactic_type(a).or_else(|| syntactic_type(b)),
		Expr::Field(_) | Expr::Const(_) => None,
	}
}

fn collect(ns: &Namespace, out: &mut HashMap<TypeId, Vec<(Ident, Ident)>>) {
	for item in ns.items.values() {
		match item {
			NamespaceItem::Inner(inner) => collect(inner, out),
			NamespaceItem::Type(t) => {
				let typed: Vec<(Ident, Ident)> = t
					.param_types
					.iter()
					.filter_map(|(param, ty)| match ty {
						TypeKind::Reference(Reference {
							ty: ReferenceKind::Generic(g),
							..
						}) if t.types.contains_key(g) => Some((param.clone(), g.clone())),
						_ => None,
					})
					.collect();
				if !typed.is_empty() {
					out.insert(t.id(), typed);
				}
			}
		}
	}
}

struct Infer<'a> {
	typed: &'a HashMap<TypeId, Vec<(Ident, Ident)>>,
}

impl Visitor for Infer<'_> {
	fn visit_reference(&mut self, r: &mut Reference, _reg: &Registry) -> Result<()> {
		let Some(typed) = r.ty.resolved().and_then(|id| self.typed.get(&id)) else {
			return Ok(());
		};
		for (param, generic) in typed {
			if r.types.contains_key(generic) {
				continue;
			}
			let inferred = r
				.args
				.iter()
				.find(|(n, _)| n == param)
				.and_then(|(_, e)| syntactic_type(e));
			if let Some(ty) = inferred {
				r.types.insert(generic.clone(), ty);
			}
		}
		Ok(())
	}
}

pub fn infer_generics(ns: &mut Namespace, reg: &Registry) -> Result<()> {
	let mut typed = HashMap::new();
	collect(ns, &mut typed);
	if typed.is_empty() {
		return Ok(());
	}
	ns.traverse(&mut Infer { typed: &typed }, TraverseSkip::empty(), reg)
}
