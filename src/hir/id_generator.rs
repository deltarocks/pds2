use std::{cell::Cell, fmt, rc::Rc};

#[derive(Clone)]
pub struct NameIdGenerator {
	id: Rc<Cell<u64>>,
}

impl NameIdGenerator {
	pub fn new() -> Self {
		Self {
			id: Rc::new(Cell::new(1)),
		}
	}
	pub fn next(&self) -> u64 {
		let next = self.id.get();
		self.id.set(next + 1);
		next
	}
}

#[derive(Clone)]
pub struct IdGenerator {
	id: Rc<Cell<u64>>,
}

impl IdGenerator {
	pub fn new() -> Self {
		Self {
			id: Rc::new(Cell::new(1)),
		}
	}
	pub fn next(&self) -> TypeId {
		let next = self.id.get();
		self.id.set(next + 1);
		TypeId(next)
	}
	pub fn next_pending(&self) -> PendingTypeId {
		let id = self.next();
		PendingTypeId(id)
	}
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct TypeId(u64);

/// Type id, which has not yet assigned to a type
#[derive(Debug)]
pub struct PendingTypeId(TypeId);
impl PendingTypeId {
	pub fn id(&self) -> TypeId {
		self.0
	}
	pub fn into_inner(self) -> TypeId {
		self.0
	}
}

impl TypeId {
	const INVALID: Self = TypeId(0);
	pub fn is_valid(&self) -> bool {
		self.0 != 0
	}
}
impl Default for TypeId {
	fn default() -> Self {
		Self::INVALID
	}
}

impl fmt::Debug for TypeId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.is_valid() {
			write!(f, "TypeId({})", self.0)
		} else {
			write!(f, "TypeId(Invalid)")
		}
	}
}
