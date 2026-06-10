//! Apply specializers desugaring pass
//!
//! Runs after path resolution. For every use-site reference to a body-accepting
//! native type (one carrying a `@specializer`), it invokes the registered
//! [`Specializer`](crate::hir::Specializer) to project the reference body into a
//! concrete list of fields, replacing the reference with an inline `Native` whose
//! `fields` are those projected fields. The specializer still owns serialization.

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, Context, Result};

use crate::{
	ast::{Ident, Path},
	hir::{
		HirSpecializerCtx, NamespaceItem, NativeType, NativeTypeVariant, Traverse, TraverseSkip,
		TypeId, TypeKind, Visitor,
	},
	specializer::Registry,
};

use super::super::Namespace;

/// Collect, for the whole tree: every type's absolute path -> id, and which ids are
/// native types with a specializer (id -> specializer name).
fn collect(
	ns: &Namespace,
	path: &mut Vec<Ident>,
	by_path: &mut HashMap<Path, TypeId>,
	spec: &mut HashMap<TypeId, Ident>,
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
					..
				}) = &t.kind
				{
					spec.insert(t.id(), s.clone());
				}
			}
		}
	}
}

struct Apply<'a> {
	by_path: &'a HashMap<Path, TypeId>,
	spec: &'a HashMap<TypeId, Ident>,
}

impl Visitor for Apply<'_> {
	fn visit_type_kind(&mut self, ty: &mut TypeKind, reg: &Registry) -> Result<()> {
		let TypeKind::Reference(r) = ty else {
			return Ok(());
		};
		let (Some(id), false) = (r.ty.resolved(), r.body.is_empty()) else {
			return Ok(());
		};
		let Some(name) = self.spec.get(&id) else {
			return Ok(());
		};
		let specializer = reg
			.asthir
			.get(name.as_str())
			.ok_or_else(|| anyhow!("no specializer implementation for {name:?}"))?;

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
		*ty = TypeKind::Native(NativeType {
			specializer: Some(name.clone()),
			variants: BTreeMap::from_iter([(
				Ident::new("default"),
				NativeTypeVariant {
					virtual_fields: vec![],
					fields,
					accepts_body: false,
					specialization,
					params,
					types,
				},
			)]),
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
