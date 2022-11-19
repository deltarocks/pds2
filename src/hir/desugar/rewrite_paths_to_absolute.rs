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
		Container, ContainerFieldType, Namespace, NamespaceItem, NativeType, Reference,
		ReferenceKind, Type, TypeKind,
	},
};

impl TypeKind {
	fn rewrite_paths_to_absolute(&mut self, current_ns: &[Ident], generics: &HashSet<Ident>) {
		match self {
			TypeKind::Container(c) => c.rewrite_paths_to_absolute(current_ns, generics),
			TypeKind::Reference(Reference {
				ty: ty @ ReferenceKind::Path(_),
				types,
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
					p.rewrite_to_absolute(current_ns);
				}
				for (_, ty) in types.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics)
				}
			}
			TypeKind::Reference(Reference {
				ty: ReferenceKind::Generic(_) | ReferenceKind::Resolved(_),
				types,
				..
			}) => {
				for (_, ty) in types.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics)
				}
			}
			TypeKind::Void => {}
			TypeKind::Native(NativeType {
				virtual_fields,
				specializer: _,
				specialization,
			}) => {
				for (_, ty) in specialization.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics)
				}
				for (_, ty) in virtual_fields.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns, generics)
				}
			}
		}
	}
}

impl Type {
	fn rewrite_paths_to_absolute(&mut self, current_ns: &[Ident]) {
		let generics = self
			.types
			.iter()
			.map(|(n, _)| n.clone())
			.collect::<HashSet<_>>();
		for (_, t) in self.types.iter_mut() {
			if let Some(t) = t {
				t.rewrite_paths_to_absolute(current_ns, &generics);
			}
		}
		self.kind.rewrite_paths_to_absolute(current_ns, &generics);
	}
}

impl Container {
	pub(crate) fn rewrite_paths_to_absolute(
		&mut self,
		current_ns: &[Ident],
		generics: &HashSet<Ident>,
	) {
		for (_, v) in self.fields.iter_mut() {
			match v {
				ContainerFieldType::Normal(n) => n.rewrite_paths_to_absolute(current_ns, generics),
				ContainerFieldType::Virtual(_, n) => {
					n.rewrite_paths_to_absolute(current_ns, generics)
				}
				ContainerFieldType::Delegate(_) => {}
			}
		}
	}
}

impl Namespace {
	fn rewrite_paths_to_absolute(&mut self, current_ns: &mut Vec<Ident>) {
		for (name, item) in self.items.iter_mut() {
			match item {
				NamespaceItem::Inner(ns) => {
					current_ns.push(name.clone());
					ns.rewrite_paths_to_absolute(current_ns);
					current_ns.pop().unwrap();
				}
				NamespaceItem::Type(t) => t.rewrite_paths_to_absolute(current_ns),
			}
		}
	}
}

pub fn rewrite_paths_to_absolute(ns: &mut Namespace) {
	ns.rewrite_paths_to_absolute(&mut Vec::new())
}
