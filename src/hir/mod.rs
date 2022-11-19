use crate::ast::{FieldRef, Ident, Path};
use std::{cell::Cell, collections::BTreeMap};
mod from_ast;
use anyhow::Result;
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
impl Traverse for NamespaceItem {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		match self {
			NamespaceItem::Inner(ns) => ns.traverse(visitor, skip)?,
			NamespaceItem::Type(t) => t.traverse(visitor, skip)?,
		}
		Ok(())
	}
}

#[derive(Debug)]
pub struct Namespace {
	pub items: BTreeMap<Ident, NamespaceItem>,
}
impl Namespace {
	pub fn retain_types<F: FnMut(&mut Type) -> bool>(&mut self, mut cond: F) {
		for (_, i) in self.items.iter_mut() {
			let NamespaceItem::Inner(ns) = i else {
				continue;
			};
			ns.retain_types(&mut cond as &mut dyn FnMut(&mut Type) -> bool);
		}
		self.items.retain(|_, t| match t {
			NamespaceItem::Inner(i) => !i.items.is_empty(),
			NamespaceItem::Type(t) => cond(t),
		});
	}
}
impl Traverse for Namespace {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		for (_, v) in self.items.iter_mut() {
			v.traverse(visitor, skip)?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone)]
pub enum ContainerFieldType {
	Normal(TypeKind),
	Virtual(FieldRef, TypeKind),
	Delegate(Ident),
}
impl Traverse for ContainerFieldType {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		match self {
			ContainerFieldType::Normal(t) => t.traverse(visitor, skip)?,
			ContainerFieldType::Virtual(f, t) => {
				visitor.visit_field_ref(f)?;
				t.traverse(visitor, skip)?
			}
			ContainerFieldType::Delegate(_) => {}
		}
		Ok(())
	}
}

#[derive(Debug, Clone)]
pub struct Container {
	pub virt: bool,
	pub fields: Vec<(Ident, ContainerFieldType)>,
	pub delegate_all: Option<Ident>,
}
impl Traverse for Container {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		for (_, f) in self.fields.iter_mut() {
			f.traverse(visitor, skip)?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone)]
pub struct NativeType {
	pub virtual_fields: Vec<(Ident, TypeKind)>,
	pub specializer: Option<Ident>,
	pub specialization: Vec<(Ident, TypeKind)>,
}
impl Traverse for NativeType {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		for (_, t) in self.virtual_fields.iter_mut() {
			t.traverse(visitor, skip)?;
		}
		for (_, t) in self.specialization.iter_mut() {
			t.traverse(visitor, skip)?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone)]
pub enum ReferenceKind {
	Resolved(TypeId),
	Path(Path),
	Generic(Ident),
}
impl ReferenceKind {
	pub fn is_path(&self) -> bool {
		matches!(self, Self::Path(_))
	}
	pub fn path(&self) -> Option<&Path> {
		match self {
			Self::Path(p) => Some(p),
			_ => None,
		}
	}
	pub fn path_mut(&mut self) -> Option<&mut Path> {
		match self {
			Self::Path(p) => Some(p),
			_ => None,
		}
	}
	pub fn is_resolved(&self) -> bool {
		matches!(self, Self::Resolved(_))
	}
	pub fn resolved(&self) -> Option<TypeId> {
		match self {
			Self::Resolved(p) => Some(*p),
			_ => None,
		}
	}
	pub fn is_generic(&self) -> bool {
		matches!(self, Self::Generic(_))
	}
	pub fn generic(&self) -> Option<Ident> {
		match self {
			Self::Generic(p) => Some(p.clone()),
			_ => None,
		}
	}
	pub fn generic_mut(&mut self) -> Option<&mut Ident> {
		match self {
			Self::Generic(p) => Some(p),
			_ => None,
		}
	}
}

#[derive(Debug, Clone)]
pub struct Reference {
	pub ty: ReferenceKind,
	pub args: Vec<(Ident, FieldRef)>,
	pub types: Vec<(Ident, TypeKind)>,
}
impl Traverse for Reference {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		visitor.visit_reference(self)?;
		for (_, f) in self.args.iter_mut() {
			visitor.visit_field_ref(f)?;
		}
		for (_, v) in self.types.iter_mut() {
			v.traverse(visitor, skip)?
		}
		Ok(())
	}
}

#[derive(Debug, Clone)]
pub enum TypeKind {
	Void,
	Container(Container),
	Reference(Reference),
	Native(NativeType),
}
impl TypeKind {
	fn assert_reference(&self) {
		match self {
			TypeKind::Reference(Reference { .. }) => {}
			n => unreachable!("should not have nested: {:?}", n),
		}
	}
}
impl Traverse for TypeKind {
	fn traverse(&mut self, visitor: &mut impl Visitor, mut skip: TraverseSkip) -> Result<()> {
		if !skip.contains(TraverseSkip::TOPLEVEL_TYPEKIND) {
			visitor.visit_type_kind(self)?;
		}
		skip -= TraverseSkip::TOPLEVEL_TYPEKIND;
		match self {
			TypeKind::Void => {}
			TypeKind::Container(c) => c.traverse(visitor, skip)?,
			TypeKind::Reference(r) => r.traverse(visitor, skip)?,
			TypeKind::Native(n) => n.traverse(visitor, skip)?,
		}
		Ok(())
	}
}

pub struct Type {
	_id: Cell<TypeId>,
	pub params: Vec<Ident>,
	pub types: Vec<(Ident, Option<TypeKind>)>,
	pub kind: TypeKind,
}
impl Traverse for Type {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		visitor.visit_type(self)?;
		for (_, t) in self.types.iter_mut() {
			if let Some(t) = t {
				visitor.visit_type_kind(t)?;
			}
		}
		self.kind.traverse(visitor, skip)
	}
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

trait Visitor {
	fn visit_field_ref(&mut self, _field: &mut FieldRef) -> Result<()> {
		Ok(())
	}
	fn visit_type_kind(&mut self, _value: &mut TypeKind) -> Result<()> {
		Ok(())
	}
	fn visit_type(&mut self, _value: &mut Type) -> Result<()> {
		Ok(())
	}
	fn visit_reference(&mut self, _value: &mut Reference) -> Result<()> {
		Ok(())
	}
}
bitflags::bitflags! {
	#[repr(transparent)]
	pub struct TraverseSkip: u32 {
		const TOPLEVEL_TYPEKIND = 0b00000001;
	}
}
trait Traverse {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()>;
}
