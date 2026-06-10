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
		FieldRefOrConst, IdGenerator, Namespace, Reference, ReferenceKind, Traverse, TraverseSkip,
		Type, TypeKind, Visitor,
	},
	specializer::Registry,
};

struct TypeReceiver {
	id: IdGenerator,
	extracted: Vec<Type>,
}

impl Visitor for TypeReceiver {
	fn visit_type_kind(&mut self, ty: &mut TypeKind, reg: &Registry) -> Result<()> {
		let mut collector = ReferenceCollector::default();
		ty.traverse(&mut collector, TraverseSkip::empty(), reg)?;

		let vty = mem::replace(ty, TypeKind::Void);
		let id = self.id.next();
		self.extracted.push(Type {
			_id: Cell::new(id),
			types: BTreeMap::new(),
			params: collector.params,
			kind: vty,
		});
		*ty = TypeKind::Reference(Reference {
			ty: ReferenceKind::Resolved(id),
			args: collector
				.args
				.into_iter()
				.map(|(i, f)| (i, FieldRefOrConst::FieldRef(f)))
				.collect(),
			types: BTreeMap::new(),
			body: vec![],
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
		let mut orig = r.clone();
		orig.up -= 1;
		let name: Ident = r.name().as_str().into();

		self.args.push((name.clone(), orig));

		*r = FieldRef {
			up: 0,
			path: BaseFieldRef(vec![FieldRefSegment::Arg(name.clone())]),
		};
		self.params.push(name);
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
			TraverseSkip::TOPLEVEL_TYPEKIND | TraverseSkip::DFS,
			reg,
		)?;
		Ok(receiver.extracted)
	})?;
	Ok(())
}
