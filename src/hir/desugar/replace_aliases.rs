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

use crate::ast::{BaseFieldRef, FieldRefSegment, Ident};
use crate::hir::{Namespace, Reference, Traverse, TraverseSkip, TypeId, TypeKind, Visitor};
use anyhow::Result;

#[derive(Debug)]
struct Alias {
	target: TypeId,
	remap_arguments: BTreeMap<Ident, BaseFieldRef>,
}

struct CollectAliasList<'t> {
	aliases: &'t mut BTreeMap<TypeId, Alias>,
	found_new: bool,
}

impl<'t> CollectAliasList<'t> {
	fn new(aliases: &'t mut BTreeMap<TypeId, Alias>) -> Self {
		Self {
			aliases,
			found_new: false,
		}
	}
}
impl Visitor for CollectAliasList<'_> {
	fn visit_type(&mut self, typ: &mut crate::hir::Type) -> Result<()> {
		if let TypeKind::Reference(Reference { ty, args, types }) = &mut typ.kind {
			assert!(types.is_empty());
			assert!(ty.is_resolved());
			let resolved = ty.resolved().unwrap();
			let remap_arguments = args
				.iter()
				.map(|(name, value)| {
					assert_eq!(value.up, 0);
					let mut refer = value.path.clone();
					let first_segment = refer.0.remove(0);
					let FieldRefSegment::Arg(arg) = first_segment else {panic!()};
					assert_eq!(&arg, name);
					(name.clone(), refer)
				})
				.collect();
			if self
				.aliases
				.insert(
					typ.id(),
					Alias {
						target: resolved,
						remap_arguments,
					},
				)
				.is_none()
			{
				self.found_new = true;
			}
		}
		Ok(())
	}
}

struct ReplaceAliasedReferences<'t> {
	aliases: &'t mut BTreeMap<TypeId, Alias>,
	replaced: bool,
}

impl<'t> ReplaceAliasedReferences<'t> {
	fn new(aliases: &'t mut BTreeMap<TypeId, Alias>) -> Self {
		Self {
			aliases,
			replaced: false,
		}
	}
}
impl Visitor for ReplaceAliasedReferences<'_> {
	fn visit_reference(&mut self, value: &mut Reference) -> Result<()> {
		assert!(value.types.is_empty());

		let resolved = value.ty.resolved().unwrap();
		if let Some(alias) = self.aliases.get(&resolved) {
			*value = Reference {
				ty: crate::hir::ReferenceKind::Resolved(alias.target),
				args: value
					.args
					.iter()
					.map(|(name, value)| {
						let mut value = value.clone();
						let remap = alias
							.remap_arguments
							.get(name)
							.expect("TODO: unused argument?");
						value.path.0.extend_from_slice(&remap.0);
						(name.clone(), value)
					})
					.collect(),
				types: Vec::new(),
			};
			self.replaced = true;
		}
		Ok(())
	}
}

pub fn replace_aliases(ir: &mut Namespace) -> Result<()> {
	let mut aliases = BTreeMap::new();
	loop {
		let mut had_changed = false;
		{
			let mut visitor = CollectAliasList::new(&mut aliases);
			ir.traverse(&mut visitor, TraverseSkip::empty())?;
			had_changed |= visitor.found_new;
		}
		{
			let mut visitor = ReplaceAliasedReferences::new(&mut aliases);
			ir.traverse(&mut visitor, TraverseSkip::empty())?;
			had_changed |= visitor.replaced;
		}
		if !had_changed {
			break;
		}
	}
	ir.retain_types(|t| !aliases.contains_key(&t.id()));
	Ok(())
}
