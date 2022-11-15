use crate::ast::{Ident, Path, Reference, Value};
use anyhow::Result;
use std::{cell::Cell, collections::BTreeMap};
mod from_ast;
pub use from_ast::parse_ast;
mod desugar;
pub use to_ast::to_ast;
mod id_generator;
mod to_ast;
pub use id_generator::{IdGenerator, TypeId};

use self::id_generator::PendingTypeId;

#[derive(Debug)]
pub enum NamespaceItem {
	Inner(Namespace),
	Type(Type),
}

#[derive(Debug)]
pub struct Namespace {
	pub items: BTreeMap<Ident, NamespaceItem>,
}

#[derive(Debug, Clone)]
pub enum ContainerFieldType {
	Normal(TypeKind),
	Virtual(Reference, TypeKind),
	Delegate(Ident),
}

#[derive(Debug, Clone)]
pub struct Container {
	pub virt: bool,
	pub fields: Vec<(Ident, ContainerFieldType)>,
	pub delegate_all: Option<Ident>,
}

#[derive(Debug, Clone)]
pub enum UnionTag {
	LoadFrom(Reference),
	Type(Box<TypeKind>),
}

#[derive(Debug, Clone)]
pub struct Union {
	pub tag: UnionTag,
	pub matches: Vec<(Value, Ident, TypeKind)>,
	pub default: Option<(Ident, Box<TypeKind>)>,
}

#[derive(Debug, Clone)]
pub enum ArrayLength {
	LoadFrom(Reference),
	Fixed(u32),
	Type(Box<TypeKind>),
}

#[derive(Debug, Clone)]
pub struct Array {
	pub length: ArrayLength,
	pub ty: Box<TypeKind>,
}

#[derive(Debug, Clone)]
pub struct NativeType {
	pub virtual_fields: Vec<(Ident, TypeKind)>,
	pub specializer: Option<Ident>,
	pub specialization: Vec<(Ident, TypeKind)>,
}

#[derive(Debug, Clone)]
pub enum TypeKind {
	Void,
	Container(Container),
	Union(Union),
	Array(Array),
	Reference {
		ty: Result<TypeId, Path>,
		args: Vec<(Ident, Reference)>,
		types: Vec<(Ident, TypeKind)>,
	},
	Native(NativeType),
}
impl TypeKind {
	fn assert_reference(&self) {
		match self {
			TypeKind::Reference { .. } => {}
			n => unreachable!("should not have nested: {:?}", n),
		}
	}
}

pub struct Type {
	_id: Cell<TypeId>,
	pub params: Vec<Ident>,
	pub types: Vec<Ident>,
	pub kind: TypeKind,
}

impl std::fmt::Debug for Type {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Type")
			.field("id", &self._id.get())
			.field("params", &self.params)
			.field("types", &self.types)
			.field("kind", &self.kind)
			.finish()
	}
}
impl Type {
	pub fn new(id: PendingTypeId, params: Vec<Ident>, kind: TypeKind) -> Self {
		Self {
			_id: Cell::new(id.into_inner()),
			params,
			types: Vec::new(),
			kind,
		}
	}
	pub fn id(&self) -> TypeId {
		let id = self._id.get();
		assert!(id.is_valid());
		id
	}
	pub fn assign_id(&self, id: &IdGenerator) {
		if !self._id.get().is_valid() {
			self._id.set(id.next())
		}
	}
	pub fn is_native(&self) -> bool {
		matches!(self.kind, TypeKind::Native(_))
	}
}
