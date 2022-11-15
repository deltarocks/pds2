//! Give all types ids desugaring pass
//!
//! This pass generates unique id for every defined type
//!
//! After this pass all type ids can be safely unwrapped

use crate::hir::{IdGenerator, Namespace};

impl Namespace {
	fn give_all_types_ids(&mut self, id: IdGenerator) {
		for (_, item) in self.items.iter_mut() {
			match item {
				crate::hir::NamespaceItem::Inner(t) => t.give_all_types_ids(id.clone()),
				crate::hir::NamespaceItem::Type(t) => {
					t.assign_id(&id);
				}
			}
		}
	}
}

pub fn give_all_types_ids(id: IdGenerator, ns: &mut Namespace) {
	ns.give_all_types_ids(id);
}
