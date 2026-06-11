use crate::ast::{FieldRef, Ident, Path};
use crate::specializer::Registry;
use std::{cell::Cell, collections::BTreeMap};
mod from_ast;
use anyhow::{anyhow, Context, Result};
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
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		match self {
			NamespaceItem::Inner(ns) => ns.traverse(visitor, skip, reg)?,
			NamespaceItem::Type(t) => t.traverse(visitor, skip, reg)?,
		}
		Ok(())
	}
}

#[derive(Debug)]
pub struct Namespace {
	pub items: BTreeMap<Ident, NamespaceItem>,
}
impl Namespace {
	/// The per-variant native types of an overload set, or `None` for a regular
	/// namespace. An overload-set namespace is only produced by `from_ast` for a
	/// `def_native` with `variant(..)` blocks: every member is a native type carrying
	/// its own item name as variant. Anything user-written has variant `"default"`.
	pub fn overload_set(&self) -> Option<Vec<&Type>> {
		let mut out = Vec::new();
		for (name, item) in self.items.iter() {
			let NamespaceItem::Type(t) = item else {
				return None;
			};
			let TypeKind::Native(n) = &t.kind else {
				return None;
			};
			if &n.variant != name || n.variant.as_str() == "default" {
				return None;
			}
			out.push(t);
		}
		(!out.is_empty()).then_some(out)
	}

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
		self.add_types(new_types);
		Ok(())
	}
}
impl Traverse for Namespace {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		for (n, v) in self.items.iter_mut() {
			let res = v.traverse(visitor, skip, reg);
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
	// Normal field = typed name
	Normal(TypeKind),
	// Virtual field = can't read, during write field reference is written instead.
	Virtual(FieldRef, TypeKind),
	//
	Delegate(Ident),
}
impl Traverse for ContainerFieldType {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		match self {
			ContainerFieldType::Normal(t) => t.traverse(visitor, skip, reg)?,
			ContainerFieldType::Virtual(f, t) => {
				visitor.visit_field_ref(f)?;
				t.traverse(visitor, skip, reg)?
			}
			ContainerFieldType::Delegate(_) => {}
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Container {
	/// Virtual containers
	pub virt: bool,
	pub fields: Vec<(Ident, ContainerFieldType)>,
	pub delegate_all: Option<Ident>,
}
impl Traverse for Container {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		for (n, f) in self.fields.iter_mut() {
			let res = f.traverse(visitor, skip, reg);
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

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub enum Value {
	Number(i128),
	String(String),
	UnionDescriminator(Ident),
}
impl Traverse for Value {
	fn traverse(
		&mut self,
		_visitor: &mut impl Visitor,
		_skip: TraverseSkip,
		_reg: &Registry,
	) -> Result<()> {
		Ok(())
	}
}

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub struct Union {
	// TODO: Should it be `tag or tagType`, and not just `tag` in HIR?
	pub tag: FieldRef,
	// TODO: Conditional value? To make Union a replacement for match statement of some sort.
	pub values: Vec<(Value, Ident, TypeKind)>,
	pub default: Option<Box<TypeKind>>,
}
impl Traverse for Union {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		visitor
			.visit_field_ref(&mut self.tag)
			.context("union tag")?;
		for (v, a, ele) in self.values.iter_mut() {
			v.traverse(visitor, skip, reg).context("union key")?;
			ele.traverse(visitor, skip, reg)
				.with_context(|| format!("union value {a:?}"))?;
		}
		if let Some(ele) = &mut self.default {
			ele.traverse(visitor, skip, reg)
				.with_context(|| format!("union default value"))?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeType {
	pub specializer: Option<Ident>,
	/// The overload this type is. A `def_native` without `variant(..)` blocks declares
	/// the single `default` variant; one with overloads is split by `from_ast` into a
	/// namespace of per-variant types, each carrying its variant name here. Use-site
	/// references select the overload during path resolution, so a separate TypeId per
	/// overload exists and MIR never sees variants.
	pub variant: Ident,

	pub virtual_fields: Vec<(Ident, TypeKind)>,
	pub fields: Vec<(Ident, TypeKind)>,
	/// whether use-sites may carry a `{ ... }` body for the specializer to process
	pub accepts_body: bool,
	/// generic bindings of this native: monomorphization substitutes them like any
	/// generic def's; unbound generics are pruned (absent entry = unbound)
	pub specialization: Vec<(Ident, TypeKind)>,
	/// the consumed use-site body, kept as metadata for codegen (e.g. bitfield sizes);
	/// the field projection already happened in `apply_specializers`
	pub body: Vec<BodyField>,
}
impl NativeType {
	/// A body-accepting native is compile-time only: every use-site is inlined by its
	/// specializer, the declaration itself never reaches MIR. (Generic natives are
	/// compile-time too, but their generics live on [`Type::types`] like any generic
	/// def, so the generic-type machinery covers them.) Natives with only value
	/// `argument(..)`s are real wire types and survive to MIR with params.
	pub fn is_template(&self) -> bool {
		self.accepts_body
	}
}

impl Traverse for NativeType {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		for (n, t) in self.virtual_fields.iter_mut() {
			t.traverse(visitor, skip, reg)
				.with_context(|| format!("virtual_field({n:?})"))?;
		}
		for (n, t) in self.fields.iter_mut() {
			t.traverse(visitor, skip, reg)
				.with_context(|| format!("field({n:?})"))?;
		}
		for (n, t) in self.specialization.iter_mut() {
			t.traverse(visitor, skip, reg)
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
pub enum FieldRefOrConst {
	FieldRef(FieldRef),
	Const(Value),
}
impl Traverse for FieldRefOrConst {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		match self {
			FieldRefOrConst::FieldRef(field_ref) => visitor.visit_field_ref(field_ref),
			FieldRefOrConst::Const(value) => value.traverse(visitor, skip, reg),
		}
	}
}
/// A single statement of a specializer body (`field(x, size: 26)`, `flag(add_player)`),
/// captured verbatim at the use-site of a body-accepting native type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BodyArg {
	Int(i128),
	Bool(bool),
	Str(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BodyField {
	/// the statement head (`field`, `flag`, ...), interpreted by the specializer
	pub kind: Ident,
	pub name: Ident,
	pub args: BTreeMap<Ident, BodyArg>,
	/// chained element type, if the body item carries one (`field(x) => u8`)
	pub ty: Option<TypeKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reference {
	pub ty: ReferenceKind,
	pub args: Vec<(Ident, FieldRefOrConst)>,
	pub types: BTreeMap<Ident, TypeKind>,
	/// body of a use-site reference to a body-accepting native, consumed by its specializer
	pub body: Vec<BodyField>,
}
impl Traverse for Reference {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		if !skip.contains(TraverseSkip::DFS) {
			visitor
				.visit_reference(self, reg)
				.with_context(|| format!("<BFS ref> {self:?}"))?;
		}

		for (_, f) in self.args.iter_mut() {
			f.traverse(visitor, skip, reg)
				.context("reference argument")?;
		}
		for (t, v) in self.types.iter_mut() {
			v.traverse(visitor, skip, reg)
				.with_context(|| format!("<ref> {{{t:?}: <here>}}"))?
		}
		for f in self.body.iter_mut() {
			if let Some(ty) = &mut f.ty {
				ty.traverse(visitor, skip, reg)
					.context("reference body field")?;
			}
		}

		if skip.contains(TraverseSkip::DFS) {
			visitor
				.visit_reference(self, reg)
				.with_context(|| format!("<DFS ref> {self:?}"))?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub enum TypeKind {
	Void,
	Container(Container),
	Union(Union),
	Reference(Reference),
	Native(NativeType),
}
impl TypeKind {
	fn is_native_template(&self) -> bool {
		matches!(self, TypeKind::Native(n) if n.is_template())
	}
}
impl Traverse for TypeKind {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		if !skip.contains(TraverseSkip::DFS) && !skip.contains(TraverseSkip::TOPLEVEL_TYPEKIND) {
			visitor.visit_type_kind(self, reg)?;
		}

		{
			let skip = skip - TraverseSkip::TOPLEVEL_TYPEKIND;
			match self {
				TypeKind::Void => {}
				TypeKind::Container(c) => c.traverse(visitor, skip, reg)?,
				TypeKind::Reference(r) => r.traverse(visitor, skip, reg)?,
				TypeKind::Native(n) => n.traverse(visitor, skip, reg)?,
				TypeKind::Union(c) => c.traverse(visitor, skip, reg)?,
			}
		}

		if skip.contains(TraverseSkip::DFS) && !skip.contains(TraverseSkip::TOPLEVEL_TYPEKIND) {
			visitor.visit_type_kind(self, reg)?;
		}
		Ok(())
	}
}

pub struct Type {
	_id: Cell<TypeId>,
	pub params: Vec<Ident>,
	pub types: BTreeMap<Ident, Option<TypeKind>>,
	pub kind: TypeKind,
	/// `export;` in the def block: keep this def in the output even when it is a
	/// trivial alias that `replace_aliases` would otherwise dissolve
	pub export: bool,
}
impl Traverse for Type {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		if skip.contains(TraverseSkip::SKIP_GENERIC_TYPES)
			&& (!self.types.is_empty() || self.kind.is_native_template())
		{
			return Ok(());
		}

		if !skip.contains(TraverseSkip::DFS) {
			visitor
				.visit_type(self, reg)
				.with_context(|| format!("<BFS type> {self:?}"))?;
		}

		for (_, t) in self.types.iter_mut() {
			if let Some(t) = t {
				visitor.visit_type_kind(t, reg)?;
			}
		}
		self.kind.traverse(visitor, skip, reg)?;

		if skip.contains(TraverseSkip::DFS) {
			visitor
				.visit_type(self, reg)
				.with_context(|| format!("<DFS type> {self:?}"))?;
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
			export: false,
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

/// Everything a specializer needs to project a reference body into fields.
pub struct HirSpecializerCtx<'a> {
	/// the selected overload variant of the native
	pub variant: &'a Ident,
	/// the use-site body statements
	pub body: &'a [BodyField],
	/// generic bindings of the reference (e.g. `Value` -> the wrapped integer type)
	pub types: &'a BTreeMap<Ident, TypeKind>,
	/// value arguments of the reference
	pub args: &'a [(Ident, FieldRefOrConst)],
	/// resolve a top-level native type by name to its id
	pub resolve: &'a dyn Fn(&str) -> Option<TypeId>,
}

impl HirSpecializerCtx<'_> {
	/// A resolved reference to a top-level native type, by name.
	pub fn native(&self, name: &str) -> Result<TypeKind> {
		let id = (self.resolve)(name).ok_or_else(|| anyhow!("native type {name:?} not found"))?;
		Ok(TypeKind::Reference(Reference {
			ty: ReferenceKind::Resolved(id),
			args: vec![],
			types: BTreeMap::new(),
			body: vec![],
		}))
	}
}

trait Visitor {
	fn enter_ns(&mut self, _ns: Ident) {}
	fn leave_ns(&mut self) {}

	fn visit_field_ref(&mut self, _field: &mut FieldRef) -> Result<()> {
		Ok(())
	}
	fn visit_type_kind(&mut self, _value: &mut TypeKind, _reg: &Registry) -> Result<()> {
		Ok(())
	}
	fn visit_type(&mut self, _value: &mut Type, _reg: &Registry) -> Result<()> {
		Ok(())
	}
	fn visit_reference(&mut self, _value: &mut Reference, _reg: &Registry) -> Result<()> {
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
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()>;
}
