//! Rewrite paths to absolute desugaring pass
//!
//! This pass rewrites all relative paths to absolute ones
//! ```pds
//! namespace(ns1) => {
//!     namespace(ns2) => {
//!         def(type2) => u8;
//!     }
//!     def(type1) => u8;
//! }
//! def(type0) => u8;
//! =>
//! namespace(ns1) => {
//!     namespace(ns2) => {
//!         def(type2) => root::ns1::ns2::u8;
//!     }
//!     def(type1) => root::ns1::u8;
//! }
//! def(type0) => root::u8;
//! ```
//!
//! It also expands simple paths to generics
//! ```pds
//! def(type0) {type(Value)} => Value (as path, ::Value);
//! =>
//! def(type0) {type(Value)} => Value (as generic, ReferenceKind::Generic)
//! ```
//!
//! After this pass every type reference is either absolute, replaced with generic, or resolved (by-id)

use std::collections::HashSet;

use crate::{
	ast::Ident,
	hir::{
		Namespace, NamespaceItem, Reference, ReferenceKind, Type,
		TypeKind,
	},
};

impl TypeKind {
	fn rewrite_paths_to_absolute(
		&mut self,
		current_ns: &[Ident],
		generics: &HashSet<Ident>,
		paths: &HashSet<Vec<Ident>>,
	) {
		match self {
			TypeKind::Reference(Reference {
				ty: ty @ ReferenceKind::Path(_),
				types,
				body,
				..
			}) => {
				// Try to resolve generic
				{
					let p = ty.path().unwrap();
					if let Some(p) = p.simple_name() {
						if generics.contains(&p) {
							*ty = ReferenceKind::Generic(p);
						}
					}
				}
				// Try to resolve absolute path
				if let Some(p) = ty.path_mut() {
					p.rewrite_to_absolute(current_ns, paths);
				}
				for (_, ty) in types.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics, paths)
				}
				for ty in body.iter_mut().filter_map(|f| f.ty.as_mut()) {
					ty.rewrite_paths_to_absolute(current_ns, generics, paths)
				}
			}
			TypeKind::Reference(Reference {
				ty: ReferenceKind::Generic(_) | ReferenceKind::Resolved(_),
				types,
				body,
				..
			}) => {
				for (_, ty) in types.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics, paths)
				}
				for ty in body.iter_mut().filter_map(|f| f.ty.as_mut()) {
					ty.rewrite_paths_to_absolute(current_ns, generics, paths)
				}
			}
			TypeKind::Native(native) => {
				// the native's generics live on the surrounding Type, already in scope
				for (_, ty) in native.specialization.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics, paths)
				}
				for (_, ty) in native.virtual_fields.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics, paths)
				}
				for (_, ty) in native.fields.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics, paths)
				}
				if let Some(decl) = &mut native.accepts_body {
					for ty in decl.default_types_mut() {
						ty.rewrite_paths_to_absolute(current_ns, generics, paths)
					}
				}
			}
		}
	}
}

impl Type {
	fn rewrite_paths_to_absolute(&mut self, current_ns: &[Ident], paths: &HashSet<Vec<Ident>>) {
		let generics = self
			.types
			.iter()
			.map(|(n, _)| n.clone())
			.collect::<HashSet<_>>();
		for (_, t) in self.types.iter_mut() {
			if let Some(t) = t {
				t.rewrite_paths_to_absolute(current_ns, &generics, paths);
			}
		}
		self.kind
			.rewrite_paths_to_absolute(current_ns, &generics, paths);
	}
}

impl Namespace {
	fn discover_paths(&mut self, current_ns: &mut Vec<Ident>, paths: &mut HashSet<Vec<Ident>>) {
		for (name, item) in self.items.iter_mut() {
			match item {
				NamespaceItem::Inner(ns) => {
					// A native overload set is referenced by its bare name (the variant
					// is selected during path resolution), so the namespace itself must
					// be a path candidate.
					if ns.overload_set().is_some() {
						let mut path = current_ns.clone();
						path.push(name.clone());
						paths.insert(path);
					}
					current_ns.push(name.clone());
					ns.discover_paths(current_ns, paths);
					current_ns.pop().unwrap();
				}
				NamespaceItem::Type(_) => {
					let mut path = current_ns.clone();
					path.push(name.clone());
					paths.insert(path);
				}
			}
		}
	}
	fn rewrite_paths_to_absolute(
		&mut self,
		current_ns: &mut Vec<Ident>,
		paths: &HashSet<Vec<Ident>>,
	) {
		for (name, item) in self.items.iter_mut() {
			match item {
				NamespaceItem::Inner(ns) => {
					current_ns.push(name.clone());
					ns.rewrite_paths_to_absolute(current_ns, paths);
					current_ns.pop().unwrap();
				}
				NamespaceItem::Type(t) => t.rewrite_paths_to_absolute(current_ns, paths),
			}
		}
	}
}

pub fn rewrite_paths_to_absolute(ns: &mut Namespace) {
	let mut paths = HashSet::new();
	ns.discover_paths(&mut Vec::new(), &mut paths);
	ns.rewrite_paths_to_absolute(&mut Vec::new(), &paths)
}
