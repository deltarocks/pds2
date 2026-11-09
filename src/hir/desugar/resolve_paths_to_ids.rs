//! Resolve paths to ids desugaring pass
//!
//! This pass resolves all absolute path type references with resolved,
//! and throws error if named type can't be resolved
//!
//! Overloaded natives (a `def_native` with `variant(..)` blocks) are split by
//! `from_ast` into a namespace of per-variant types (`union::tagged`,
//! `union::inline`). A reference to the bare native name selects the overload here,
//! by shape: the names of supplied value arguments (lowercase), generic bindings
//! (uppercase), and whether a body is present, are matched against each variant's
//! declared signature; the most specific match wins, ambiguity is an error.
//! `name::variant` paths select explicitly through the plain path map.
//!
//! After this every type reference is either generic, or resolved (by-id)

use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeSet, HashMap};

use crate::{
	ast::{Ident, Path},
	hir::{
		id_generator::TypeId, Namespace, NativeType, Reference, ReferenceKind, Type, TypeKind,
	},
};

/// Declared shape of one native overload, used to match a use-site against it.
struct VariantSig {
	name: Ident,
	id: TypeId,
	params: BTreeSet<Ident>,
	types: BTreeSet<Ident>,
	accepts_body: bool,
}
impl VariantSig {
	fn new(t: &Type, n: &NativeType) -> Self {
		Self {
			name: n.variant.clone(),
			id: t.id(),
			params: t.params.iter().cloned().collect(),
			types: t.types.keys().cloned().collect(),
			accepts_body: n.accepts_body,
		}
	}
}

#[derive(Default)]
struct Paths {
	plain: HashMap<Path, TypeId>,
	/// bare native name -> its overload variants
	overloads: HashMap<Path, Vec<VariantSig>>,
}

/// Pick the overload a use-site reference matches. A variant is a candidate when every
/// name the reference supplies is declared by it and a body is only passed to a
/// body-accepting variant; the most specific candidate (most names matched) wins.
fn select<'s>(r: &Reference, variants: &'s [VariantSig]) -> Result<&'s VariantSig> {
	let provided_args: BTreeSet<&Ident> = r.args.iter().map(|(n, _)| n).collect();
	let provided_types: BTreeSet<&Ident> = r.types.keys().collect();
	let has_body = !r.body.is_empty();

	let mut candidates: Vec<(&VariantSig, usize)> = Vec::new();
	for v in variants {
		let args_ok = provided_args.iter().all(|n| v.params.contains(*n));
		let types_ok = provided_types.iter().all(|n| v.types.contains(*n));
		let body_ok = !has_body || v.accepts_body;
		if args_ok && types_ok && body_ok {
			// More declared names matched => more specific => preferred.
			candidates.push((v, provided_args.len() + provided_types.len()));
		}
	}
	let best = candidates.iter().map(|(_, s)| *s).max().ok_or_else(|| {
		anyhow!(
			"no native overload matches use-site (args: {provided_args:?}, types: {provided_types:?}, body: {has_body})"
		)
	})?;
	let mut top = candidates.iter().filter(|(_, s)| *s == best);
	let (sig, _) = top.next().unwrap();
	if let Some((other, _)) = top.next() {
		bail!(
			"ambiguous native overload: variants {:?} and {:?} both match",
			sig.name,
			other.name
		);
	}
	Ok(sig)
}

impl TypeKind {
	fn resolve_paths_to_ids(&mut self, paths: &Paths) -> Result<()> {
		match self {
			TypeKind::Reference(r) => {
				if let ReferenceKind::Path(path) = &r.ty {
					let id = if let Some(id) = paths.plain.get(path) {
						*id
					} else if let Some(variants) = paths.overloads.get(path) {
						select(r, variants)
							.with_context(|| format!("overload of {path:?}"))?
							.id
					} else {
						bail!("path not found: {path:?}")
					};
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
				if let crate::hir::UnionTag::Inline(t) = &mut u.tag {
					t.resolve_paths_to_ids(paths)?
				}
				for (_, _, ele) in u.values.iter_mut() {
					ele.resolve_paths_to_ids(paths)?
				}
				if let Some(d) = u.default.as_mut() {
					d.resolve_paths_to_ids(paths)?
				}
			}
			TypeKind::Native(NativeType {
				specializer: _,
				variant: _,
				virtual_fields,
				fields,
				accepts_body: _,
				specialization,
				body: _,
				const_args: _,
				is_void: _,
				scalar: _,
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
	fn resolve_paths_to_ids(&mut self, paths: &Paths) -> Result<()> {
		for (_, t) in self.types.iter_mut() {
			if let Some(t) = t {
				t.resolve_paths_to_ids(paths)?;
			}
		}
		self.kind.resolve_paths_to_ids(paths)
	}
}

impl Namespace {
	fn collect_path_ids(&self, out: &mut Paths, current_path: &mut Vec<Ident>) {
		for (n, v) in self.items.iter() {
			match v {
				crate::hir::NamespaceItem::Inner(i) => {
					if let Some(variants) = i.overload_set() {
						let old = out.overloads.insert(
							Path {
								absolute: true,
								path: current_path.clone(),
								item: n.clone(),
							},
							variants
								.into_iter()
								.map(|t| {
									let TypeKind::Native(n) = &t.kind else {
										unreachable!("checked by overload_set");
									};
									VariantSig::new(t, n)
								})
								.collect(),
						);
						assert!(old.is_none(), "no duplicates possible");
					}
					current_path.push(n.clone());
					i.collect_path_ids(out, current_path);
					current_path.pop();
				}
				crate::hir::NamespaceItem::Type(t) => {
					let old = out.plain.insert(
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
	fn resolve_paths_to_ids(&mut self, paths: &Paths) -> Result<()> {
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
	let mut paths = Paths::default();
	ns.collect_path_ids(&mut paths, &mut Vec::new());

	ns.resolve_paths_to_ids(&paths)
}
