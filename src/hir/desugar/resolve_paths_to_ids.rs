//! Resolve paths to ids desugaring pass
//!
//! This pass resolves all absolute path type references with resolved,
//! and throws error if named type can't be resolved
//!
//! After this every type reference is either generic, or resolved (by-id)

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;

use crate::{
	ast::{Ident, Path},
	hir::{id_generator::TypeId, Namespace, NativeType, ReferenceKind, Type, TypeKind},
};

impl TypeKind {
	fn resolve_paths_to_ids(&mut self, paths: &HashMap<Path, TypeId>) -> Result<()> {
		match self {
			TypeKind::Reference(r) => {
				if let ReferenceKind::Path(path) = &r.ty {
					let id = *paths.get(path).ok_or_else(|| {
						anyhow!("path not found: {:?}, possible paths: {:?}", path, paths)
					})?;
					r.ty = ReferenceKind::Resolved(id);
				}
				for (_, ty) in r.types.iter_mut() {
					ty.resolve_paths_to_ids(paths)?;
				}
				for f in r.body.iter_mut() {
					if let Some(ty) = &mut f.ty {
						ty.resolve_paths_to_ids(paths)?;
					}
				}
			}
			TypeKind::Container(c) => {
				for (_, v) in c.fields.iter_mut() {
					match v {
						crate::hir::ContainerFieldType::Normal(t) => {
							t.resolve_paths_to_ids(paths)?
						}
						crate::hir::ContainerFieldType::Virtual(_, p) => {
							p.resolve_paths_to_ids(paths)?
						}
						crate::hir::ContainerFieldType::Delegate(_) => {}
					}
				}
			}
			TypeKind::Union(u) => {
				for (_, _, ele) in u.values.iter_mut() {
					ele.resolve_paths_to_ids(paths)?
				}
			}
			TypeKind::Native(NativeType {
				virtual_fields,
				fields,
				specializer: _,
				specialization,
				..
			}) => {
				for (_, v) in specialization.iter_mut() {
					v.resolve_paths_to_ids(paths)?;
				}
				for (_, v) in virtual_fields.iter_mut() {
					v.resolve_paths_to_ids(paths)?;
				}
				for (_, v) in fields.iter_mut() {
					v.resolve_paths_to_ids(paths)?;
				}
			}
			TypeKind::Void => {}
		}
		Ok(())
	}
}

impl Type {
	fn resolve_paths_to_ids(&mut self, paths: &HashMap<Path, TypeId>) -> Result<()> {
		for (_, t) in self.types.iter_mut() {
			if let Some(t) = t {
				t.resolve_paths_to_ids(paths)?;
			}
		}
		self.kind.resolve_paths_to_ids(paths)
	}
}

impl Namespace {
	fn collect_path_ids(&self, out: &mut HashMap<Path, TypeId>, current_path: &mut Vec<Ident>) {
		for (n, v) in self.items.iter() {
			match v {
				crate::hir::NamespaceItem::Inner(i) => {
					current_path.push(n.clone());
					i.collect_path_ids(out, current_path);
					current_path.pop();
				}
				crate::hir::NamespaceItem::Type(t) => {
					let old = out.insert(
						Path {
							absolute: true,
							path: current_path.clone(),
							item: n.clone(),
						},
						t.id(),
					);
					assert!(old.is_none(), "no duplicates possible");
				}
			}
		}
	}
	fn resolve_paths_to_ids(&mut self, paths: &HashMap<Path, TypeId>) -> Result<()> {
		for (n, v) in self.items.iter_mut() {
			match v {
				crate::hir::NamespaceItem::Inner(i) => i
					.resolve_paths_to_ids(paths)
					.with_context(|| format!("namespace {:?}", n))?,
				crate::hir::NamespaceItem::Type(t) => {
					t.resolve_paths_to_ids(paths)
						.with_context(|| format!("item {:?}", n))?;
				}
			}
		}

		Ok(())
	}
}

pub fn resolve_paths_to_ids(ns: &mut Namespace) -> Result<()> {
	let mut paths = HashMap::new();
	ns.collect_path_ids(&mut paths, &mut Vec::new());

	ns.resolve_paths_to_ids(&paths)
}
