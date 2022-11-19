//! Replaces trivial aliased types
//!
//! ```rust
//! def(myu8) => u8;
//!
//! def(root) => container {
//!     field(v) => myu8;
//! };
//! =>
//! def(root) => container {
//!     field(v) => u8;
//! };
//! ```

use std::collections::BTreeMap;

use crate::ast::{Ident, BaseReference, ReferenceSegment};
use crate::hir::{ContainerFieldType, Namespace, NamespaceItem, Type, TypeId, TypeKind};
use anyhow::Result;

#[derive(Debug)]
struct Alias {
	target: TypeId,
	remap_arguments: BTreeMap<Ident, BaseReference>,
}

impl TypeKind {
	fn replace_aliases(&mut self, aliases: &BTreeMap<TypeId, Alias>) {
		match self {
			TypeKind::Void => {}
			TypeKind::Container(c) => {
				for (_, field) in c.fields.iter_mut() {
					match field {
						ContainerFieldType::Normal(f) => f.replace_aliases(aliases),
						ContainerFieldType::Virtual(_, t) => t.replace_aliases(aliases),
						ContainerFieldType::Delegate(_) => {}
					}
				}
			}
			TypeKind::Union(_) => todo!(),
			TypeKind::Reference { ty, args, types } => {
				assert!(types.is_empty());
				let resolved = ty.resolved().unwrap();
				if let Some(alias) = aliases.get(&resolved) {
					*self = TypeKind::Reference {
						ty: crate::hir::ReferenceKind::Resolved(alias.target),
						args: args.iter()
							.map(|(name, value)| {
								let mut value = value.clone();
								let remap = alias.remap_arguments.get(name).expect("TODO: unused argument?");
								value.path.0.extend_from_slice(&remap.0);
								(name.clone(), value)
							})
							.collect(),
						types: Vec::new(),
					}
				}
			}
			TypeKind::Native(n) => {
				for (_, t) in n.virtual_fields.iter_mut() {
					t.replace_aliases(aliases)
				}
				for (_, t) in n.specialization.iter_mut() {
					t.replace_aliases(aliases)
				}
			}
		}
	}
}

impl Type {
	fn replace_aliases(&mut self, aliases: &BTreeMap<TypeId, Alias>) {
		self.kind.replace_aliases(aliases)
	}
}

impl Namespace {
	fn collect_aliases(&mut self, out: &mut BTreeMap<TypeId, Alias>) -> bool {
		let mut found = false;
		for (_, item) in self.items.iter_mut() {
			match item {
				NamespaceItem::Inner(ns) => found |= ns.collect_aliases(out),
				NamespaceItem::Type(t) => {
					if let TypeKind::Reference { ty, args, types } = &mut t.kind {
						assert!(types.is_empty());
						assert!(ty.is_resolved());
						let resolved = ty.resolved().unwrap();
						let remap_arguments = args
							.iter()
							.map(|(name, value)| {
								assert_eq!(value.up, 0);
								let mut refer = value.path.clone();
								let first_segment = refer.0.remove(0);
								let ReferenceSegment::Arg(arg) = first_segment else {panic!()};
								assert_eq!(&arg, name);
								(name.clone(), refer)
							})
							.collect();
						if out
							.insert(
								t.id(),
								Alias {
									target: resolved,
									remap_arguments,
								},
							)
							.is_none()
						{
							found = true;
						}
					}
				}
			}
		}
		found
	}
	fn replace_aliases(&mut self, aliases: &BTreeMap<TypeId, Alias>) {
		for (_, item) in self.items.iter_mut() {
			match item {
				NamespaceItem::Inner(ns) => ns.replace_aliases(aliases),
				NamespaceItem::Type(t) => t.replace_aliases(aliases),
			}
		}
	}
	fn remove_aliased(&mut self, aliases: &BTreeMap<TypeId, Alias>) {
		self.items.retain(|_, v| match v {
			NamespaceItem::Inner(ns) => {
				ns.remove_aliased(aliases);
				!ns.items.is_empty()
			}
			NamespaceItem::Type(t) => !aliases.contains_key(&t.id()),
		})
	}
}

pub fn replace_aliases(ir: &mut Namespace) -> Result<()> {
	let mut aliases = BTreeMap::new();
	loop {
		let found = ir.collect_aliases(&mut aliases);
		ir.replace_aliases(&aliases);
		ir.remove_aliased(&aliases);
		if !found {
			break;
		}
	}
	Ok(())
}
