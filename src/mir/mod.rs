mod from_hir;
pub use from_hir::from_hir;
use std::{cell::Cell, collections::BTreeMap, ops::Index};
mod pass;

use crate::{
	ast::{FieldRefSegment, Ident, Path},
	hir::TypeId,
};

#[derive(Debug)]
pub struct Type {
	pub target: TypeId,
	pub args: Vec<(Ident, FieldOrConst)>,
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
pub struct NativeType {
	// Which user defined function should be called to generate parser for this type,
	// and which def_native overload variant it should dispatch on ("default" unless
	// overloads are declared)
	pub specializer: Option<(Ident, Ident)>,
	// Which monomorphized generics this opaque type has; unbound generics are absent.
	// Wire-time value inputs arrive as reference args binding the def's params.
	pub specialization: Vec<(Ident, Type)>,
	pub virtual_fields: Vec<(Ident, Type)>,
	pub fields: Vec<(Ident, Type)>,
	// The consumed use-site body (codegen metadata: bitfield sizes, flag order, ...)
	pub body: Vec<crate::hir::BodyField>,
	// Constant configuration arguments from the use-site (registryEntryHolder baseName)
	pub const_args: Vec<(Ident, crate::hir::Value)>,
	// carries no data (the deduplicated canonical void)
	pub is_void: bool,
}

/// Where a union reads its discriminant from.
#[derive(Debug)]
pub enum UnionTag {
	/// a field of the enclosing container, passed in as an argument
	External(Field),
	/// read inline by the union itself, immediately before the value
	Inline(Type),
}

#[derive(Debug)]
pub enum TypeKind {
	Container {
		virt: bool,
		fields: Vec<ContainerField>,
		delegate_all: Option<Ident>,
	},
	Union {
		tag: UnionTag,
		tag_type: TypeId,
		matches: Vec<(Value, Ident, Type)>,
		default: Option<(Ident, Type)>,
	},
	Native(NativeType),
	/// An exported named alias to another type, possibly binding its arguments
	/// (`def(root_const_field) { export; } => array(size: 3) => varint`). Generators
	/// emit a forwarding type.
	Alias(Type),
	Void,
}

impl TypeKind {
	pub fn is_native(&self) -> bool {
		matches!(self, TypeKind::Native(_))
	}
}

#[derive(Debug, Clone)]
pub enum Value {
	Unsigned(u128),
	Signed(i128),
	Bool(bool),
	String(String),
	UnionDescriminator(Ident),
}

#[derive(Debug)]
pub struct TypeMap(pub BTreeMap<TypeId, Definition>);
impl Index<TypeId> for TypeMap {
	type Output = Definition;

	fn index(&self, index: TypeId) -> &Self::Output {
		self.0.get(&index).as_ref().unwrap()
	}
}
impl TypeMap {
	/// Whether a type carries no wire data: the canonical `is_void` native, or a bare
	/// `Void` kind (when no `is_void` native was declared to deduplicate to).
	pub fn is_void(&self, id: TypeId) -> bool {
		match &self[id].kind {
			TypeKind::Void => true,
			TypeKind::Native(n) => n.is_void,
			_ => false,
		}
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
	pub segment: FieldRefSegment,
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

#[derive(Clone, Debug)]
pub enum FieldOrConst {
	Field(Field),
	Const(Value),
}
impl FieldOrConst {
	fn as_field(&self) -> Option<&Field> {
		match self {
			FieldOrConst::Field(field) => Some(field),
			FieldOrConst::Const(_) => None,
		}
	}
}
