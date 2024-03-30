use crate::ast::{FieldRef, Ident, Path};
use std::{cell::Cell, collections::BTreeMap};
mod from_ast;
use anyhow::{Context, Result};
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
	fn add_types(&mut self, types: impl IntoIterator<Item = (Ident, Type)>) {
		let mut i = 0;
		for (n, t) in types {
			let name = n.as_str().to_owned();
			let mut derived_name;
			loop {
				derived_name = Ident::new(&format!("{name}_{i}"));
				if self.items.contains_key(&derived_name) {
					i += 1;
				} else {
					break;
				}
			}
			self.items.insert(derived_name, NamespaceItem::Type(t));
		}
	}
	pub fn derive_types<F: FnMut(&mut Type) -> Result<Vec<Type>>>(
		&mut self,
		mut generator: F,
	) -> Result<()> {
		let mut new_types = Vec::new();
		for (n, i) in self.items.iter_mut() {
			match i {
				NamespaceItem::Inner(i) => {
					i.derive_types(
						&mut generator as &mut dyn FnMut(&mut Type) -> Result<Vec<Type>>,
					)?;
				}
				NamespaceItem::Type(t) => {
					let new = generator(t)?;
					new_types.extend(new.into_iter().map(|t| (n.clone(), t)))
				}
			}
		}
		self.add_types(new_types.into_iter());
		Ok(())
	}
}
impl Traverse for Namespace {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		for (n, v) in self.items.iter_mut() {
			let res = v.traverse(visitor, skip);
			match v {
				NamespaceItem::Inner(_) => res.with_context(|| format!("namespace({n:?})"))?,
				NamespaceItem::Type(_) => res.with_context(|| format!("def({n:?})"))?,
			}
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Container {
	pub virt: bool,
	pub fields: Vec<(Ident, ContainerFieldType)>,
	pub delegate_all: Option<Ident>,
}
impl Traverse for Container {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		for (n, f) in self.fields.iter_mut() {
			let res = f.traverse(visitor, skip);
			match f {
				ContainerFieldType::Normal(_) => res.with_context(|| format!("field({n:?})"))?,
				ContainerFieldType::Virtual(_, _) => {
					res.with_context(|| format!("virtual_field({n:?})"))?
				}
				ContainerFieldType::Delegate(_) => todo!(),
			}
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeType {
	pub virtual_fields: Vec<(Ident, TypeKind)>,
	pub specializer: Option<Ident>,
	pub specialization: Vec<(Ident, TypeKind)>,
}
impl Traverse for NativeType {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		for (n, t) in self.virtual_fields.iter_mut() {
			t.traverse(visitor, skip)
				.with_context(|| format!("virtual_field({n:?})"))?;
		}
		for (n, t) in self.specialization.iter_mut() {
			t.traverse(visitor, skip)
				.with_context(|| format!("type({n:?})"))?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reference {
	pub ty: ReferenceKind,
	pub args: Vec<(Ident, FieldRef)>,
	pub types: BTreeMap<Ident, TypeKind>,
}
impl Traverse for Reference {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		if !skip.contains(TraverseSkip::DFS) {
			visitor.visit_reference(self).context("<ref>")?;
		}

		for (_, f) in self.args.iter_mut() {
			visitor.visit_field_ref(f)?;
		}
		for (t, v) in self.types.iter_mut() {
			v.traverse(visitor, skip)
				.with_context(|| format!("<ref> {{{t:?}: <here>}}"))?
		}

		if skip.contains(TraverseSkip::DFS) {
			visitor.visit_reference(self).context("<ref>")?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq)]
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
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		if !skip.contains(TraverseSkip::DFS) && !skip.contains(TraverseSkip::TOPLEVEL_TYPEKIND) {
			visitor.visit_type_kind(self)?;
		}

		{
			let skip = skip - TraverseSkip::TOPLEVEL_TYPEKIND;
			match self {
				TypeKind::Void => {}
				TypeKind::Container(c) => c.traverse(visitor, skip)?,
				TypeKind::Reference(r) => r.traverse(visitor, skip)?,
				TypeKind::Native(n) => n.traverse(visitor, skip)?,
			}
		}

		if skip.contains(TraverseSkip::DFS) && !skip.contains(TraverseSkip::TOPLEVEL_TYPEKIND) {
			visitor.visit_type_kind(self)?;
		}
		Ok(())
	}
}

pub struct Type {
	_id: Cell<TypeId>,
	pub params: Vec<Ident>,
	pub types: BTreeMap<Ident, Option<TypeKind>>,
	pub kind: TypeKind,
}
impl Traverse for Type {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()> {
		if !self.types.is_empty() && skip.contains(TraverseSkip::SKIP_GENERIC_TYPES) {
			return Ok(());
		}

		if !skip.contains(TraverseSkip::DFS) {
			visitor.visit_type(self)?;
		}

		for (_, t) in self.types.iter_mut() {
			if let Some(t) = t {
				visitor.visit_type_kind(t)?;
			}
		}
		self.kind.traverse(visitor, skip)?;

		if skip.contains(TraverseSkip::DFS) {
			visitor.visit_type(self)?;
		}
		Ok(())
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
	pub fn new(
		id: PendingTypeId,
		params: Vec<Ident>,
		kind: TypeKind,
		types: BTreeMap<Ident, Option<TypeKind>>,
	) -> Self {
		Self {
			_id: Cell::new(id.into_inner()),
			params,
			types,
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
	fn enter_ns(&mut self, _ns: Ident) {}
	fn leave_ns(&mut self) {}

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
	#[derive(Clone, Copy)]
	pub struct TraverseSkip: u32 {
		const TOPLEVEL_TYPEKIND = 1;
		const DFS = 2;
		const SKIP_GENERIC_TYPES = 4;
	}
}
trait Traverse {
	fn traverse(&mut self, visitor: &mut impl Visitor, skip: TraverseSkip) -> Result<()>;
}
