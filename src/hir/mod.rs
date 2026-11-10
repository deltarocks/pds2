use crate::ast::{BinOp, FieldRef, Ident, Path};
use crate::specializer::Registry;
use std::{cell::Cell, collections::BTreeMap};
mod from_ast;
use anyhow::{anyhow, bail, ensure, Context, Result};
use std::collections::BTreeSet;
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

#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub enum Value {
	Number(i128),
	Bool(bool),
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
	pub accepts_body: Option<BodyDecl>,
	/// generic bindings of this native: monomorphization substitutes them like any
	/// generic def's; unbound generics are pruned (absent entry = unbound)
	pub specialization: Vec<(Ident, TypeKind)>,
	/// the consumed use-site body, kept as metadata for codegen (e.g. bitfield sizes);
	/// the field projection already happened in `apply_specializers`
	pub body: Vec<BodyField>,
	/// constant configuration arguments captured at the use-site when the reference
	/// was inlined (`registryEntryHolder(baseName: "patternId") { ... }`)
	pub const_args: Vec<(Ident, Value)>,
	pub ref_args: Vec<(Ident, Expr)>,
	pub arg_types: Vec<(Ident, TypeKind)>,
	pub arg_virtuals: Vec<Ident>,
	/// `is_void;` in the def_native block: the type carries no data.
	pub is_void: bool,
	pub scalar: Option<Ident>,
	pub attrs: BTreeMap<Ident, Vec<Ident>>,
}
impl NativeType {
	/// A body-accepting native is compile-time only: every use-site is inlined by its
	/// specializer, the declaration itself never reaches MIR. (Generic natives are
	/// compile-time too, but their generics live on [`Type::types`] like any generic
	/// def, so the generic-type machinery covers them.) Natives with only value
	/// `argument(..)`s are real wire types and survive to MIR with params.
	pub fn is_template(&self) -> bool {
		self.accepts_body.is_some()
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
		for (n, r) in self.ref_args.iter_mut() {
			r.traverse(visitor, skip, reg)
				.with_context(|| format!("argument {n:?}"))?;
		}
		for (n, t) in self.arg_types.iter_mut() {
			t.traverse(visitor, skip, reg)
				.with_context(|| format!("argument {n:?} type"))?;
		}
		for f in self.body.iter_mut() {
			for (n, a) in f.args.iter_mut() {
				a.traverse(visitor, skip, reg)
					.with_context(|| format!("body item argument {n:?}"))?;
			}
		}
		if let Some(decl) = &mut self.accepts_body {
			for t in decl.default_types_mut() {
				t.traverse(visitor, skip, reg)
					.context("body item default type")?;
			}
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
pub enum Expr {
	Field(FieldRef),
	Const(Value),
	Binary(BinOp, Box<Expr>, Box<Expr>),
	Cast(Box<Expr>, Box<TypeKind>),
}
impl Expr {
	pub fn types_mut(&mut self) -> Vec<&mut TypeKind> {
		match self {
			Expr::Field(_) | Expr::Const(_) => vec![],
			Expr::Binary(_, a, b) => {
				let mut out = a.types_mut();
				out.extend(b.types_mut());
				out
			}
			Expr::Cast(e, t) => {
				let mut out = e.types_mut();
				out.push(t);
				out
			}
		}
	}
}
impl Traverse for Expr {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		match self {
			Expr::Field(field_ref) => visitor.visit_field_ref(field_ref),
			Expr::Const(value) => value.traverse(visitor, skip, reg),
			Expr::Binary(_, a, b) => {
				a.traverse(visitor, skip, reg)?;
				b.traverse(visitor, skip, reg)
			}
			Expr::Cast(e, t) => {
				e.traverse(visitor, skip, reg)?;
				t.traverse(visitor, skip, reg)
			}
		}
	}
}
/// A single statement of a specializer body (`field(x, size: 26)`, `flag(add_player)`),
/// captured verbatim at the use-site of a body-accepting native type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BodyField {
	/// the statement head (`field`, `flag`, ...), interpreted by the specializer
	pub kind: Ident,
	pub name: Option<Ident>,
	pub args: BTreeMap<Ident, Expr>,
	/// chained element type, if the body item carries one (`field(x) => u8`)
	pub ty: Option<TypeKind>,
}
impl BodyField {
	pub fn expect_name(&self) -> Result<&Ident> {
		self.name
			.as_ref()
			.ok_or_else(|| anyhow!("body item {:?} needs a name", self.kind))
	}
	pub fn constant(&self, name: &str) -> Option<&Value> {
		match self.args.get(&Ident::new(name))? {
			Expr::Const(v) => Some(v),
			_ => None,
		}
	}
}
impl Traverse for BodyField {
	fn traverse(
		&mut self,
		visitor: &mut impl Visitor,
		skip: TraverseSkip,
		reg: &Registry,
	) -> Result<()> {
		for (n, a) in self.args.iter_mut() {
			a.traverse(visitor, skip, reg)
				.with_context(|| format!("body item argument {n:?}"))?;
		}
		if let Some(ty) = &mut self.ty {
			ty.traverse(visitor, skip, reg)
				.context("reference body field")?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct BodyItemDecl {
	pub anonymous: bool,
	pub args: Vec<(Ident, Option<Value>)>,
	pub value: Option<Option<TypeKind>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct BodyDecl(pub Vec<(Ident, BodyItemDecl)>);
impl BodyDecl {
	pub fn item(&self, kind: &Ident) -> Option<&BodyItemDecl> {
		self.0.iter().find(|(k, _)| k == kind).map(|(_, d)| d)
	}
	pub fn default_types_mut(&mut self) -> impl Iterator<Item = &mut TypeKind> {
		self.0
			.iter_mut()
			.filter_map(|(_, d)| d.value.as_mut().and_then(Option::as_mut))
	}
	pub fn check(&self, body: &mut [BodyField]) -> Result<()> {
		let mut names = BTreeSet::new();
		let mut anonymous = BTreeSet::new();
		for f in body.iter_mut() {
			let kind = f.kind.clone();
			let decl = self.item(&kind).ok_or_else(|| {
				anyhow!(
					"unknown body item {kind:?}, expected one of {:?}",
					self.0.iter().map(|(k, _)| k).collect::<Vec<_>>()
				)
			})?;
			match (&f.name, decl.anonymous) {
				(Some(n), false) => ensure!(names.insert(n.clone()), "duplicate body item name {n:?}"),
				(None, true) => ensure!(anonymous.insert(kind.clone()), "{kind:?} given twice"),
				(None, false) => bail!("{kind:?} needs a name"),
				(Some(n), true) => bail!("{kind:?} takes no name, got {n:?}"),
			}
			for n in f.args.keys() {
				ensure!(
					decl.args.iter().any(|(a, _)| a == n),
					"{kind:?} has no argument {n:?}"
				);
			}
			for (a, default) in &decl.args {
				if f.args.contains_key(a) {
					continue;
				}
				let default = default
					.clone()
					.ok_or_else(|| anyhow!("{kind:?} needs argument {a:?}"))?;
				f.args.insert(a.clone(), Expr::Const(default));
			}
			match (&decl.value, &mut f.ty) {
				(None, Some(_)) => bail!("{kind:?} takes no `=> type`"),
				(Some(None), None) => bail!("{kind:?} needs `=> type`"),
				(Some(Some(default)), ty @ None) => *ty = Some(default.clone()),
				_ => {}
			}
		}
		Ok(())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reference {
	pub ty: ReferenceKind,
	pub args: Vec<(Ident, Expr)>,
	pub types: BTreeMap<Ident, TypeKind>,
	/// body of a use-site reference to a body-accepting native, consumed by its specializer
	pub body: Vec<BodyField>,
}
impl Reference {
	pub fn nested_types_mut(&mut self) -> Vec<&mut TypeKind> {
		let mut out: Vec<&mut TypeKind> = self.types.values_mut().collect();
		for (_, a) in self.args.iter_mut() {
			out.extend(a.types_mut());
		}
		for f in self.body.iter_mut() {
			for a in f.args.values_mut() {
				out.extend(a.types_mut());
			}
			if let Some(t) = &mut f.ty {
				out.push(t);
			}
		}
		out
	}
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
			f.traverse(visitor, skip, reg)?;
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
				TypeKind::Reference(r) => r.traverse(visitor, skip, reg)?,
				TypeKind::Native(n) => n.traverse(visitor, skip, reg)?,
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
	pub param_types: BTreeMap<Ident, TypeKind>,
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
		for (_, t) in self.param_types.iter_mut() {
			visitor.visit_type_kind(t, reg)?;
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
			param_types: BTreeMap::new(),
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
	pub args: &'a [(Ident, Expr)],
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
