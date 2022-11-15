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
//! After this pass every defined path is absolute

use crate::{
	ast::Ident,
	hir::{
		Container, ContainerFieldType, Namespace, NamespaceItem, NativeTypeKind, Type, TypeKind,
		Union,
	},
};

impl TypeKind {
	fn rewrite_paths_to_absolute(&mut self, current_ns: &[Ident]) {
		match self {
			TypeKind::Container(c) => c.rewrite_paths_to_absolute(current_ns),
			TypeKind::Union(u) => u.rewrite_paths_to_absolute(current_ns),
			TypeKind::Array(a) => a.ty.rewrite_paths_to_absolute(current_ns),
			TypeKind::Reference {
				ty: Err(p), types, ..
			} => {
				p.rewrite_to_absolute(current_ns);
				for (_, ty) in types.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns)
				}
			}
			TypeKind::Reference { .. } => unreachable!(),
			TypeKind::Void => {}
			TypeKind::Native(NativeTypeKind::Opaque {
				virtual_fields,
				specializer: _,
				specialization,
			}) => {
				assert!(specialization.is_empty(), "only set after monomorphization");
				for (_, ty) in virtual_fields.iter_mut() {
					ty.rewrite_paths_to_absolute(current_ns)
				}
			}
			TypeKind::Native(_) => {}
		}
	}
}

impl Type {
	fn rewrite_paths_to_absolute(&mut self, current_ns: &[Ident]) {
		self.kind.rewrite_paths_to_absolute(current_ns);
	}
}

impl Union {
	pub fn rewrite_paths_to_absolute(&mut self, current_ns: &[Ident]) {
		for (_, _, t) in self.matches.iter_mut() {
			t.rewrite_paths_to_absolute(current_ns)
		}
		if let Some((_, d)) = &mut self.default {
			d.rewrite_paths_to_absolute(current_ns)
		}
	}
}
impl Container {
	pub(crate) fn rewrite_paths_to_absolute(&mut self, current_ns: &[Ident]) {
		for (_, v) in self.fields.iter_mut() {
			match v {
				ContainerFieldType::Normal(n) => n.rewrite_paths_to_absolute(current_ns),
				ContainerFieldType::Virtual(_, n) => n.rewrite_paths_to_absolute(current_ns),
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
