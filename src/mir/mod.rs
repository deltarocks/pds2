mod from_hir;
pub use from_hir::from_hir;
use std::{cell::Cell, collections::BTreeMap, ops::Index};
mod pass;

use crate::{
	ast::{Ident, Path, ReferenceSegment},
	hir::TypeId,
};

#[derive(Debug)]
pub struct Type {
	pub target: TypeId,
	pub args: Vec<(Ident, Field)>,
}

#[derive(Debug)]
pub struct Definition {
	pub name: Path,
	pub params: Vec<(Ident, TypeId)>,
	pub kind: TypeKind,
}

#[derive(Debug)]
pub struct ContainerField {
	pub name: Ident,
	pub ty: Type,
	pub load_from: Option<Field>,
}

#[derive(Debug)]
pub enum NativeTypeKind {
	Int(u32),
	Uint(u32),
	Opaque {
		// Which user defined function should be called to generate parser for this type
		specializer: Option<Ident>,
		// Which monomorphized generics this opaque type has
		specialization: Vec<(Ident, Type)>,
		virtual_fields: Vec<(Ident, Type)>,
	},
}

#[derive(Debug)]
pub enum TypeKind {
	Container {
		virt: bool,
		fields: Vec<ContainerField>,
		delegate_all: Option<Ident>,
	},
	Union {
		tag: Field,
		matches: Vec<(Value, Ident, Type)>,
		default: Option<(Ident, Type)>,
	},
	FixedSizeArray {
		size: u32,
		t: Type,
	},
	DynamicArray {
		size_ref: Field,
		t: Type,
	},
	Native(NativeTypeKind),
	Void,
}

impl TypeKind {
	pub fn is_native(&self) -> bool {
		matches!(self, TypeKind::Native(_))
	}
}

#[derive(Debug)]
pub enum Value {
	Unsigned(u128),
	Signed(i128),
	String(String),
}

#[derive(Debug)]
pub struct TypeMap(pub BTreeMap<TypeId, Definition>);
impl Index<TypeId> for TypeMap {
	type Output = Definition;

	fn index(&self, index: TypeId) -> &Self::Output {
		self.0.get(&index).as_ref().unwrap()
	}
}

#[derive(Debug, Clone)]
pub enum FieldItemKind {
	/// Can only be first
	Argument,
	Field,
	Virtual,
}

#[derive(Clone)]
pub struct FieldItem {
	pub from: Cell<TypeId>,
	pub segment: ReferenceSegment,
	pub to: Cell<TypeId>,
}

impl std::fmt::Debug for FieldItem {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.from.get().is_valid() {
			write!(f, "<{:?}>", self.from.get())?;
		}
		write!(f, "{:?}", self.segment)?;
		if self.to.get().is_valid() {
			write!(f, "<{:?}>", self.to.get())?;
		}
		Ok(())
	}
}

#[derive(Clone)]
pub struct Field(pub Vec<FieldItem>);

impl std::fmt::Debug for Field {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, ".")?;
		for item in &self.0 {
			write!(f, "/{:?}", item)?;
		}
		Ok(())
	}
}
