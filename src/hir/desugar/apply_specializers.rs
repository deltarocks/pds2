//! Apply specializers desugaring pass
//!
//! Runs after path resolution, before monomorphization. Every use-site reference to
//! a *body-accepting* specializer native (`body;` in the `def_native`) invokes the
//! registered [`AstHir`](crate::specializer::AstHir) implementation to project the
//! use-site body into a concrete list of fields, replacing the reference with an
//! inline `Native` whose `fields` are those projected fields. The specializer still
//! owns serialization; the overload variant is recorded on the inline native so
//! generators can dispatch per-overload.
//!
//! Specializer natives *without* a body (e.g. `array`) are not touched here: they
//! are ordinary generic types, monomorphized like any other, with use-site value
//! arguments staying on the reference.

use std::collections::HashMap;

use anyhow::{anyhow, ensure, Context, Result};

use crate::{
	ast::{Ident, Path},
	hir::{
		HirSpecializerCtx, NamespaceItem, NativeType, Traverse, TraverseSkip, TypeId, TypeKind,
		Visitor,
	},
	specializer::Registry,
};

use super::super::Namespace;

/// Collect, for the whole tree: every type's absolute path -> id, and which ids are
/// body-accepting specializer natives (id -> (specializer name, overload variant)).
fn collect(
	ns: &Namespace,
	path: &mut Vec<Ident>,
	by_path: &mut HashMap<Path, TypeId>,
	spec: &mut HashMap<TypeId, (Ident, Ident)>,
) {
	for (name, item) in &ns.items {
		match item {
			NamespaceItem::Inner(inner) => {
				path.push(name.clone());
				collect(inner, path, by_path, spec);
				path.pop();
			}
			NamespaceItem::Type(t) => {
				by_path.insert(
					Path {
						absolute: true,
						path: path.clone(),
						item: name.clone(),
					},
					t.id(),
				);
				if let TypeKind::Native(NativeType {
					specializer: Some(s),
					variant,
					accepts_body: true,
					..
				}) = &t.kind
				{
					spec.insert(t.id(), (s.clone(), variant.clone()));
				}
			}
		}
	}
}

struct Apply<'a> {
	by_path: &'a HashMap<Path, TypeId>,
	spec: &'a HashMap<TypeId, (Ident, Ident)>,
}

impl Visitor for Apply<'_> {
	fn visit_type_kind(&mut self, ty: &mut TypeKind, reg: &Registry) -> Result<()> {
		let TypeKind::Reference(r) = ty else {
			return Ok(());
		};
		let Some(id) = r.ty.resolved() else {
			return Ok(());
		};
		let Some((name, variant)) = self.spec.get(&id) else {
			return Ok(());
		};
		let specializer = reg
			.asthir
			.get(name.as_str())
			.ok_or_else(|| anyhow!("no specializer implementation for {name:?}"))?;
		// The inline result has nowhere to keep value arguments; none of the
		// body-accepting natives take them so far.
		ensure!(
			r.args.is_empty(),
			"body-accepting native does not take value arguments: {:?}",
			r.args
		);

		let resolve = |n: &str| {
			self.by_path
				.get(&Path {
					absolute: true,
					path: vec![],
					item: Ident::new(n),
				})
				.copied()
		};
		let fields = specializer
			.fields(&HirSpecializerCtx {
				variant,
				body: &r.body,
				types: &r.types,
				args: &r.args,
				resolve: &resolve,
			})
			.with_context(|| format!("specializer {name:?}"))?;

		let specialization = r
			.types
			.iter()
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		// keep the consumed body around: codegen needs its metadata (sizes, flags)
		let body = std::mem::take(&mut r.body);
		*ty = TypeKind::Native(NativeType {
			specializer: Some(name.clone()),
			variant: variant.clone(),
			virtual_fields: vec![],
			fields,
			accepts_body: false,
			specialization,
			body,
		});
		Ok(())
	}
}

pub fn apply_specializers(ns: &mut Namespace, reg: &Registry) -> Result<()> {
	let mut by_path = HashMap::new();
	let mut spec = HashMap::new();
	collect(ns, &mut Vec::new(), &mut by_path, &mut spec);

	if spec.is_empty() {
		return Ok(());
	}

	ns.traverse(
		&mut Apply {
			by_path: &by_path,
			spec: &spec,
		},
		TraverseSkip::DFS,
		reg,
	)
}
