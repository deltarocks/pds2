mod expr;
mod from_hir;
pub use expr::{Construction, Expr};
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
	pub args: Vec<(Ident, Expr)>,
}

#[derive(Debug)]
pub struct Definition {
	pub name: Path,
	pub params: Vec<(Ident, TypeId)>,
	pub param_types: Vec<(Ident, TypeId)>,
	pub kind: TypeKind,
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
	pub body: Vec<BodyItem>,
	// Constant configuration arguments from the use-site (registryEntryHolder baseName)
	pub const_args: Vec<(Ident, crate::hir::Value)>,
	pub ref_args: Vec<(Ident, Expr)>,
	pub arg_types: Vec<(Ident, TypeId)>,
	pub arg_virtuals: Vec<Ident>,
	pub is_void: bool,
	pub scalar: Option<Ident>,
	pub attrs: BTreeMap<Ident, Vec<Ident>>,
}
impl NativeType {
	pub fn attr(&self, name: &str) -> Option<&[Ident]> {
		self.attrs.get(&Ident::new(name)).map(Vec::as_slice)
	}
}

#[derive(Debug, Clone)]
pub struct BodyItem {
	pub kind: Ident,
	pub name: Option<Ident>,
	pub args: Vec<(Ident, Expr)>,
}
impl BodyItem {
	pub fn expect_name(&self) -> anyhow::Result<&Ident> {
		self.name
			.as_ref()
			.ok_or_else(|| anyhow::anyhow!("body item {:?} needs a name", self.kind))
	}
	pub fn arg(&self, name: &str) -> Option<&Expr> {
		self.args
			.iter()
			.find(|(n, _)| n.as_str() == name)
			.map(|(_, v)| v)
	}
	pub fn constant(&self, name: &str) -> Option<&Value> {
		match self.arg(name)? {
			Expr::Const(v) => Some(v),
			_ => None,
		}
	}
}

#[derive(Debug)]
pub enum TypeKind {
	Native(NativeType),
	/// An exported named alias to another type, possibly binding its arguments
	/// (`def(root_const_field) { export; } => array(size: 3) => varint`). Generators
	/// emit a forwarding type.
	Alias(Type),
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
	Float(f64),
	Bool(bool),
	String(String),
	Bytes(Vec<u8>),
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
	pub fn is_void(&self, id: TypeId) -> bool {
		match &self[id].kind {
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

