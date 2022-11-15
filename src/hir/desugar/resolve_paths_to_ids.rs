//! Resolve paths to ids desugaring pass
//!
//! This pass resolves all RelativeNamedReference's to Reference's,
//! and throws error if named type can't be resolved
//!
//! After this pass no RelativeNamedReference should exist in IR

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;

use crate::{
	ast::{Ident, Path},
	hir::{id_generator::TypeId, Namespace, NativeType, Type, TypeKind},
};

impl TypeKind {
	fn resolve_paths_to_ids(
		&mut self,
		paths: &std::collections::HashMap<Path, TypeId>,
	) -> Result<()> {
		fn fix_path(paths: &HashMap<Path, TypeId>, t: &mut TypeKind) -> Result<()> {
			match t {
				TypeKind::Reference { ty, types, .. } if ty.is_err() => {
					*ty = Ok(*paths.get(&ty.clone().err().unwrap()).ok_or_else(|| {
						anyhow!(
							"path not found: {:?}, possible paths: {:?}",
							ty.clone().err().unwrap(),
							paths
						)
					})?);
					for (_, ty) in types.iter_mut() {
						ty.resolve_paths_to_ids(paths)?;
					}
				}
				TypeKind::Reference { .. } => {}
				_ => unreachable!(),
			}
			Ok(())
		}
		match self {
			TypeKind::Container(c) => {
				for (_, v) in c.fields.iter_mut() {
					match v {
						crate::hir::ContainerFieldType::Normal(t) => fix_path(paths, t)?,
						crate::hir::ContainerFieldType::Virtual(_, p) => fix_path(paths, p)?,
						crate::hir::ContainerFieldType::Delegate(_) => {}
					}
				}
			}
			TypeKind::Union(u) => {
				for (_, _, t) in u.matches.iter_mut() {
					fix_path(paths, t)?
				}
				if let Some((_, def)) = &mut u.default {
					fix_path(paths, def)?
				}
			}
			TypeKind::Array(t) => fix_path(paths, &mut t.ty)?,
			TypeKind::Reference { ty, types, .. } if ty.is_err() => {
				*ty = Ok(*paths
					.get(&ty.clone().err().unwrap())
					.ok_or_else(|| anyhow!("path not found: {:?}", ty.clone().err().unwrap()))?);
				for (_, ty) in types.iter_mut() {
					fix_path(paths, ty)?;
				}
			}
			TypeKind::Reference { .. } => {}
			TypeKind::Native(NativeType {
				virtual_fields,
				specializer: _,
				specialization,
			}) => {
				assert!(
					specialization.is_empty(),
					"only set during monomorphization"
				);
				for field in virtual_fields.iter_mut() {
					fix_path(paths, &mut field.1)?;
				}
			}
			TypeKind::Void => {}
		}
		Ok(())
	}
}

impl Type {
	fn resolve_paths_to_ids(
		&mut self,
		paths: &std::collections::HashMap<Path, TypeId>,
	) -> Result<()> {
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
