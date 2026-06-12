//! Deduplicate void types
//!
//! Inline `Void` kinds (a `variant(x, match: 0);` union arm without a value, an
//! empty `default`) would each be extracted into their own zero-sized def, producing
//! piles of identical `Ty = ()` modules — one per mapper arm. A native can declare
//! `is_void;` to mark itself as the canonical no-data type; this pass rewrites every
//! inline `Void` kind into a reference to the first such native, and redirects
//! references to any other `is_void` native to it too.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;

use crate::{
	hir::{NamespaceItem, Reference, ReferenceKind, Traverse, TraverseSkip, TypeId, TypeKind},
	specializer::Registry,
};

use super::super::Namespace;

fn collect(ns: &Namespace, out: &mut Vec<TypeId>) {
	for (_, item) in &ns.items {
		match item {
			NamespaceItem::Inner(inner) => collect(inner, out),
			NamespaceItem::Type(t) => {
				if matches!(&t.kind, TypeKind::Native(n) if n.is_void) {
					out.push(t.id());
				}
			}
		}
	}
}

struct Dedupe<'a> {
	canonical: TypeId,
	others: &'a HashSet<TypeId>,
}
impl Visitor for Dedupe<'_> {
	fn visit_type_kind(&mut self, ty: &mut TypeKind, _reg: &Registry) -> Result<()> {
		if matches!(ty, TypeKind::Void) {
			*ty = TypeKind::Reference(Reference {
				ty: ReferenceKind::Resolved(self.canonical),
				args: vec![],
				types: BTreeMap::new(),
				body: vec![],
			});
		}
		Ok(())
	}
	fn visit_reference(&mut self, value: &mut Reference, _reg: &Registry) -> Result<()> {
		if let Some(id) = value.ty.resolved() {
			if self.others.contains(&id) {
				value.ty = ReferenceKind::Resolved(self.canonical);
			}
		}
		Ok(())
	}
}

use crate::hir::Visitor;

pub fn deduplicate_voids(ns: &mut Namespace, reg: &Registry) -> Result<()> {
	let mut voids = Vec::new();
	collect(ns, &mut voids);
	let Some((&canonical, rest)) = voids.split_first() else {
		// no marked void native: keep inline Void kinds as-is
		return Ok(());
	};
	let others: HashSet<TypeId> = rest.iter().copied().collect();

	ns.traverse(
		&mut Dedupe {
			canonical,
			others: &others,
		},
		TraverseSkip::DFS,
		reg,
	)?;
	ns.retain_types(|t| !others.contains(&t.id()));
	Ok(())
}
