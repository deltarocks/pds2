//! Extract nested types desugaring pass
//!
//! This pass removes expression nesting, moving nested expressions to namespace root,
//! and replacing usage with type reference
//!
//! ```pds
//! def(struct_a) => container {
//!     field(field_b) => container {
//!         field(field_c) => u8;
//!     };
//! };
//!
//! =>
//!
//! def(struct_a) => container {
//!     field(field_b) => root::struct_a_field_b;
//! };
//! def(struct_a_field_b) => container {
//!     field(field_c) => u8;
//! };
//! ```
//!
//! Because types can refer to parent struct fields - this pass also rewrites references to arguments
//! ```pds
//! def(struct_a) => container {
//!     field(size) => usize;
//!     field(data) => array(length: ../size);
//! };
//!
//! =>
//!
//! def(struct_a) => container {
//!     field(size) => usize;
//!     field(data) => struct_a_data(size: ./size);
//! };
//! def(struct_a_data) => array(length: ./@size)
//! ```
//!
//! After this pass types can only have other references as thier children, and there is no references to parent structs

use std::{cell::Cell, collections::BTreeMap, mem};

use anyhow::Result;

use crate::{
	ast::{BaseFieldRef, FieldRef, FieldRefSegment, Ident},
	hir::{
		Expr, IdGenerator, Namespace, Reference, ReferenceKind, Traverse, TraverseSkip,
		Type, TypeKind, Visitor,
	},
	specializer::Registry,
};

struct TypeReceiver {
	id: IdGenerator,
	extracted: Vec<Type>,
}

struct HasEscapingRefs<'t>(&'t mut bool);
impl Visitor for HasEscapingRefs<'_> {
	fn visit_field_ref(&mut self, r: &mut FieldRef) -> Result<()> {
		*self.0 |= r.up > 0;
		Ok(())
	}
}

impl Visitor for TypeReceiver {
	fn visit_type_kind(&mut self, ty: &mut TypeKind, reg: &Registry) -> Result<()> {
		// A plain reference is already flat; re-extracting it (e.g. on the second
		// extraction pass) would orphan its scope-rooted arguments. Only pull it out
		// when it still carries scope-crossing refs that need rewiring into params.
		if matches!(ty, TypeKind::Reference(_)) {
			let mut escaping = false;
			ty.traverse(&mut HasEscapingRefs(&mut escaping), TraverseSkip::empty(), reg)?;
			if !escaping {
				return Ok(());
			}
		}
		let mut collector = ReferenceCollector::default();
		ty.traverse(&mut collector, TraverseSkip::empty(), reg)?;

		let id = self.id.next();
		let vty = mem::replace(
			ty,
			TypeKind::Reference(Reference {
				ty: ReferenceKind::Resolved(id),
				args: collector
					.args
					.into_iter()
					.map(|(i, f)| (i, Expr::Field(f)))
					.collect(),
				types: BTreeMap::new(),
				body: vec![],
			}),
		);
		self.extracted.push(Type {
			param_types: BTreeMap::new(),
			_id: Cell::new(id),
			types: BTreeMap::new(),
			params: collector.params,
			kind: vty,
			export: false,
		});
		Ok(())
	}
}

#[derive(Default)]
struct ReferenceCollector {
	args: Vec<(Ident, FieldRef)>,
	params: Vec<Ident>,
}
impl Visitor for ReferenceCollector {
	fn visit_field_ref(&mut self, r: &mut FieldRef) -> Result<()> {
		if r.up == 0 {
			return Ok(());
		}
		if matches!(r.path.0.first(), Some(FieldRefSegment::Arg(_))) {
			// already rewired to a param by a previous (inner) extraction
			return Ok(());
		}
		let mut orig = r.clone();
		orig.up = orig.up.saturating_sub(1);
		let mut name: Ident = r.name().as_str().into();

		// reuse the param if the same source ref was already captured; uniquify the
		// name on collision with a different source
		if let Some((existing, _)) = self
			.args
			.iter()
			.find(|(n, o)| o == &orig && n.as_str().starts_with(name.as_str()))
		{
			name = existing.clone();
		} else {
			let mut i = 0;
			while self.params.contains(&name) {
				name = Ident::new(&format!("{}_{i}", r.name().as_str()));
				i += 1;
			}
			self.args.push((name.clone(), orig));
			self.params.push(name.clone());
		}

		*r = FieldRef {
			up: 0,
			path: BaseFieldRef(vec![FieldRefSegment::Arg(name.clone())]),
		};
		Ok(())
	}
}

pub fn extract_nested_types(id: IdGenerator, ir: &mut Namespace, reg: &Registry) -> Result<()> {
	ir.derive_types(|t| {
		let mut receiver = TypeReceiver {
			id: id.clone(),
			extracted: vec![],
		};
		t.traverse(
			&mut receiver,
			// SKIP_GENERIC_TYPES also skips template `def_native`s, whose specializer
			// placeholders are not real nested types to extract.
			TraverseSkip::TOPLEVEL_TYPEKIND | TraverseSkip::DFS | TraverseSkip::SKIP_GENERIC_TYPES,
			reg,
		)?;
		Ok(receiver.extracted)
	})?;
	Ok(())
}
