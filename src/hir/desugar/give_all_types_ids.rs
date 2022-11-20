//! Give all types ids desugaring pass
//!
//! This pass generates unique id for every defined type
//!
//! After this pass all type ids can be safely unwrapped

use anyhow::Result;

use crate::hir::{IdGenerator, Namespace, Traverse, TraverseSkip, Type, Visitor};

struct AssignIds(IdGenerator);
impl Visitor for AssignIds {
	fn visit_type(&mut self, ty: &mut Type) -> Result<()> {
		ty.assign_id(&self.0);
		Ok(())
	}
}

pub fn give_all_types_ids(id: IdGenerator, ns: &mut Namespace) -> Result<()> {
	ns.traverse(&mut AssignIds(id), TraverseSkip::empty())
}
