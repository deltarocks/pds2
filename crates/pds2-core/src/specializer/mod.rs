use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, Context as _, Result};
use genco::lang::{rust, JavaScript, Lang, Rust};
use genco::prelude::*;
use genco::Tokens;

pub mod decode;
pub mod encode;
pub mod expr;
pub mod reflect;
pub mod ts;

use encode::Encoder;
use expr::{Elaborator, ValueModel};

use crate::ast::Ident;
use crate::hir::{HirSpecializerCtx, TypeId};
use crate::{hir, mir};

/// Object-safe: registered per native, invoked during AST->HIR to produce type fields.
pub trait AstHir {
	fn fields(&self, _ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		Ok(vec![])
	}
	fn project(&self, ctx: &HirSpecializerCtx) -> Result<Projection> {
		Ok(Projection {
			fields: self.fields(ctx)?,
			..Default::default()
		})
	}
}

#[derive(Default)]
pub struct Projection {
	pub fields: Vec<(Ident, hir::TypeKind)>,
	pub virtual_fields: Vec<(Ident, hir::TypeKind)>,
	pub arg_virtuals: Vec<Ident>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeneratorKind {
	Rust,
	Typescript,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
	I8,
	U8,
	I16,
	U16,
	I32,
	U32,
	I64,
	U64,
	F32,
	F64,
	Bool,
	String,
	Bytes,
}

impl Scalar {
	pub fn is_number(self) -> bool {
		!matches!(self, Self::Bool | Self::String | Self::Bytes)
	}
	pub fn parse(s: &str) -> Option<Self> {
		Some(match s {
			"bool" => Self::Bool,
			"string" => Self::String,
			"bytes" => Self::Bytes,
			"i8" => Self::I8,
			"u8" => Self::U8,
			"i16" => Self::I16,
			"u16" => Self::U16,
			"i32" => Self::I32,
			"u32" => Self::U32,
			"i64" => Self::I64,
			"u64" => Self::U64,
			"f32" => Self::F32,
			"f64" => Self::F64,
			_ => return None,
		})
	}
}

/// Name resolution for generated code: genco imports for another pds type's items,
/// so cross-module imports are collected and deduplicated by genco.
pub trait Names<L: Lang> {
	fn ty(&self, id: TypeId) -> Tokens<L>;
	fn serialize(&self, id: TypeId) -> Tokens<L>;
	fn deserialize(&self, id: TypeId) -> Tokens<L>;
	fn size_of(&self, id: TypeId) -> Tokens<L>;
	fn boxed(&self, owner: TypeId, slot: &Ident) -> bool;
	fn slot_ty(&self, owner: TypeId, slot: &Ident, target: TypeId) -> Tokens<L>;
	fn slot_new(&self, owner: TypeId, slot: &Ident, value: Tokens<L>) -> Tokens<L>;
	fn scalar(&self, id: TypeId) -> Option<Scalar>;
	fn item(&self, id: TypeId, name: &str) -> Tokens<L>;
	fn owned(&self, id: TypeId) -> Tokens<L> {
		self.ty(id)
	}
	fn view_fn(&self, id: TypeId) -> Tokens<L> {
		self.item(id, "view")
	}
	fn into_view_fn(&self, id: TypeId) -> Tokens<L> {
		self.item(id, "into_view")
	}
	fn runtime(&self, item: &str) -> Tokens<L>;
	fn field(&self, _owner: TypeId, name: &str) -> String {
		name.to_owned()
	}
	fn variant(&self, _owner: TypeId, name: &str) -> String {
		name.to_owned()
	}
	fn member_type(&self, _owner: TypeId, name: &str) -> String {
		name.to_owned()
	}
	fn arg(&self, _owner: TypeId, name: &str) -> String {
		name.to_owned()
	}
	fn guarded(&self, _id: TypeId) -> bool {
		false
	}
}

#[derive(Debug, Default)]
pub struct Members {
	pub fields: Vec<Ident>,
	pub variants: Vec<Ident>,
	pub types: Vec<Ident>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewPolicy {
	Opaque,
	Composite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoxCost {
	Low,
	High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Embed {
	Indirect,
	Fixed,
	Boxable(BoxCost),
}

pub trait SlotVisitor {
	fn slot(&mut self, name: &Ident, target: TypeId, embed: Embed);
}

pub trait NativeLayout {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor);
	fn members(&self, _native: &mir::NativeType) -> Members {
		Members::default()
	}
}

/// Everything a native codegen needs to emit the module contents for one concrete
/// (monomorphized) native instance.
pub struct NativeCodegenCtx<'a, L: Lang> {
	pub id: TypeId,
	/// the selected def_native overload
	pub variant: &'a Ident,
	pub native: &'a mir::NativeType,
	/// value params of the def (`argument(size)` + closure-converted captures), with
	/// resolved types
	pub params: &'a [(Ident, TypeId)],
	pub names: &'a dyn Names<L>,
	/// resolve a top-level type by name (e.g. `u8` for raw byte IO)
	pub resolve: &'a dyn Fn(&str) -> Option<TypeId>,
	/// the `pub struct Args<'a> { ... }` declaration for this def's params (empty
	/// tokens when there are none)
	pub args_struct: &'a dyn Fn() -> Tokens<L>,
	/// extra `deserialize` parameter (`args: Args<'_>,`) when the def has params
	pub args_param: &'a dyn Fn() -> Tokens<L>,
	/// the `Args { ... },` to pass when calling `deserialize` of the given type-use
	/// (empty tokens when the target takes no params); arg refs resolve against this
	/// def's own `args`
	pub pass_args: &'a dyn Fn(&mir::Type) -> Tokens<L>,
	pub types: &'a mir::TypeMap,
	pub elab: &'a Elaborator<'a>,
	pub exprs: &'a [mir::Expr],
	pub cast_to: &'a [TypeId],
	pub virtual_used: &'a dyn Fn(&str) -> bool,
	pub read: &'a dyn Fn(&mir::Expr, bool) -> Tokens<L>,
	pub lower: &'a dyn Fn(&mir::Expr, bool) -> Result<Tokens<L>>,
	pub view: bool,
	pub take: bool,
	pub type_name: Option<&'a str>,
	pub declared: &'a Cell<bool>,
}

pub struct LowerCx<'a, L: Lang> {
	pub names: &'a dyn Names<L>,
	pub types: &'a mir::TypeMap,
	pub lower: &'a dyn Fn(&mir::Expr) -> Result<Tokens<L>>,
}

impl<L: Lang> NativeCodegenCtx<'_, L> {
	pub fn specialization_use(&self, name: &str) -> Result<&mir::Type> {
		specialization_use(self.native, name)
			.ok_or_else(|| anyhow!("generic {name:?} is not bound"))
	}
	pub fn specialization(&self, name: &str) -> Result<TypeId> {
		Ok(self.specialization_use(name)?.target)
	}
	pub fn boxed(&self, slot: &Ident) -> bool {
		self.names.boxed(self.id, slot)
	}
	pub fn slot_ty(&self, slot: &Ident, target: TypeId) -> Tokens<L> {
		self.names.slot_ty(self.id, slot, target)
	}
	pub fn slot_new(&self, slot: &Ident, value: Tokens<L>) -> Tokens<L> {
		self.names.slot_new(self.id, slot, value)
	}
	pub fn field(&self, name: &str) -> String {
		self.names.field(self.id, name)
	}
	pub fn variant(&self, name: &str) -> String {
		self.names.variant(self.id, name)
	}
	pub fn member_type(&self, name: &str) -> String {
		self.names.member_type(self.id, name)
	}
	pub fn arg(&self, name: &str) -> String {
		self.names.arg(self.id, name)
	}
	pub fn runtime(&self, item: &str) -> Tokens<L> {
		self.names.runtime(item)
	}
	/// a full `deserialize(input, args...)` call expression for a type-use,
	/// threading the use's bound arguments (assumes `input`/`args` in scope)
	pub fn deserialize_call(&self, u: &mir::Type) -> Tokens<L> {
		let de = self.names.deserialize(u.target);
		let pass = (self.pass_args)(u);
		quote!($de(input, $pass))
	}
	/// the declared type of a virtual field, if the variant declares it
	pub fn virtual_field(&self, name: &str) -> Option<TypeId> {
		self.native
			.virtual_fields
			.iter()
			.find(|(n, _)| n.as_str() == name)
			.map(|(_, t)| t.target)
	}
}

impl NativeCodegenCtx<'_, Rust> {
	pub fn lt(&self) -> rust::Tokens {
		if self.view {
			quote!(<'i>)
		} else {
			quote!()
		}
	}
	pub fn this(&self) -> rust::Tokens {
		quote!(Ty$(self.lt()))
	}
	pub fn nominal(&self) -> rust::Tokens {
		match self.type_name {
			Some(name) => {
				self.declared.set(true);
				quote!($name$(self.lt()))
			}
			None => self.this(),
		}
	}
	pub fn input(&self) -> rust::Tokens {
		quote!($(rust::import("crate::pds", "Input"))$(self.lt()))
	}
	pub fn owned_this(&self) -> rust::Tokens {
		self.names.owned(self.id)
	}
	pub fn into_path(&self) -> rust::Tokens {
		if self.take {
			self.owned_this()
		} else {
			quote!(Ty)
		}
	}
	pub fn into_view_fn(&self, body: rust::Tokens) -> rust::Tokens {
		let owned = self.owned_this();
		if self.take {
			quote! {
				pub fn into_view(_0: &mut $(&owned)) -> $(&owned) {
					$body
				}
			}
		} else {
			quote! {
				pub fn into_view<'i>(_0: &'i mut $(&owned)) -> Ty<'i> {
					$body
				}
			}
		}
	}
}

/// A token type a generator produces, tied to its dedicated codegen storage in
/// [`Registry`] (one field per language: trait objects of different `T` can't share
/// a map).
pub trait LanguageTokens: Lang + Sized {
	fn language() -> GeneratorKind;
	fn storage(reg: &Registry) -> &HashMap<String, Box<dyn NativeCodegen<Self>>>;
	fn storage_mut(reg: &mut Registry) -> &mut HashMap<String, Box<dyn NativeCodegen<Self>>>;
}

impl LanguageTokens for Rust {
	fn language() -> GeneratorKind {
		GeneratorKind::Rust
	}
	fn storage(reg: &Registry) -> &HashMap<String, Box<dyn NativeCodegen<Self>>> {
		&reg.codegen_rust
	}
	fn storage_mut(reg: &mut Registry) -> &mut HashMap<String, Box<dyn NativeCodegen<Self>>> {
		&mut reg.codegen_rust
	}
}

impl LanguageTokens for JavaScript {
	fn language() -> GeneratorKind {
		GeneratorKind::Typescript
	}
	fn storage(reg: &Registry) -> &HashMap<String, Box<dyn NativeCodegen<Self>>> {
		&reg.codegen_ts
	}
	fn storage_mut(reg: &mut Registry) -> &mut HashMap<String, Box<dyn NativeCodegen<Self>>> {
		&mut reg.codegen_ts
	}
}

/// Object-safe per language: registered per specializer, produces the module
/// contents for a concrete native instance in the generator's language.
pub trait NativeCodegen<L: Lang> {
	fn generate(&self, ctx: &NativeCodegenCtx<L>) -> Result<Tokens<L>>;
	fn inline_ty(
		&self,
		_id: TypeId,
		_native: &mir::NativeType,
		_names: &dyn Names<L>,
	) -> Option<Tokens<L>> {
		None
	}
	fn construct(
		&self,
		_cx: &LowerCx<L>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<Tokens<L>> {
		bail!("{id:?} has no construction {c:?}")
	}
	fn cast(
		&self,
		_cx: &LowerCx<L>,
		id: TypeId,
		_native: &mir::NativeType,
		_value: Tokens<L>,
		to: TypeId,
	) -> Result<Tokens<L>> {
		bail!("{id:?} has no cast to {to:?}")
	}
	fn nullable(&self, _native: &mir::NativeType) -> bool {
		false
	}
	fn wraps_nullable(&self, _native: &mir::NativeType, _slot: &Ident) -> bool {
		false
	}
	fn view_policy(&self) -> ViewPolicy {
		ViewPolicy::Opaque
	}
	fn view_borrows(&self, _types: &mir::TypeMap, _native: &mir::NativeType) -> bool {
		false
	}
	fn view_take(&self) -> bool {
		false
	}
	fn view_heap(&self) -> bool {
		false
	}
	fn view_moves(&self) -> bool {
		false
	}
	fn into_view(&self, _ctx: &NativeCodegenCtx<L>) -> Result<Option<Tokens<L>>> {
		Ok(None)
	}
}

pub const VALUE: &str = "Value";

pub fn specialization_use<'n>(native: &'n mir::NativeType, name: &str) -> Option<&'n mir::Type> {
	native
		.specialization
		.iter()
		.find(|(n, _)| n.as_str() == name)
		.map(|(_, t)| t)
}

pub fn visit_specialization(
	native: &mir::NativeType,
	name: &str,
	embed: Embed,
	v: &mut dyn SlotVisitor,
) {
	if let Some((n, t)) = native
		.specialization
		.iter()
		.find(|(n, _)| n.as_str() == name)
	{
		v.slot(n, t.target, embed);
	}
}

pub fn visit_fields(native: &mir::NativeType, embed: Embed, v: &mut dyn SlotVisitor) {
	for (n, t) in &native.fields {
		v.slot(n, t.target, embed);
	}
}

pub fn vec_of_value(native: &mir::NativeType, names: &dyn Names<Rust>) -> Option<rust::Tokens> {
	let value = specialization_use(native, VALUE)?.target;
	Some(quote!(Vec<$(names.ty(value))>))
}

pub fn byte_scalar(types: &mir::TypeMap, value: TypeId) -> Option<&'static str> {
	match crate::generator::common::scalar_of(types, value) {
		Some(Scalar::U8) => Some("u8"),
		Some(Scalar::I8) => Some("i8"),
		_ => None,
	}
}

pub fn view_vec(ctx: &NativeCodegenCtx<Rust>, value: TypeId) -> rust::Tokens {
	quote! {
		pub fn view<'i>(_0: &'i $(ctx.owned_this())) -> Ty<'i> {
			_0.iter().map(|e| $(ctx.names.view_fn(value))(e)).collect()
		}
	}
}

pub fn into_view_vec(ctx: &NativeCodegenCtx<Rust>, value: TypeId) -> rust::Tokens {
	ctx.into_view_fn(quote!(_0.iter_mut().map(|e| $(ctx.names.into_view_fn(value))(e)).collect()))
}

#[derive(Default)]
pub struct Registry {
	asthir: HashMap<String, Box<dyn AstHir>>,
	layout: HashMap<String, Box<dyn NativeLayout>>,
	codegen_rust: HashMap<String, Box<dyn NativeCodegen<Rust>>>,
	codegen_ts: HashMap<String, Box<dyn NativeCodegen<JavaScript>>>,
	value: HashMap<String, Box<dyn ValueModel>>,
	encoders: HashMap<String, Box<dyn Encoder>>,
}

fn insert<T: ?Sized>(map: &mut HashMap<String, Box<T>>, facet: &str, name: &str, item: Box<T>) {
	assert!(
		map.insert(name.to_owned(), item).is_none(),
		"{facet} {name:?} is registered twice"
	);
}

impl Registry {
	pub fn register_encoder(&mut self, name: &str, encoder: impl Encoder + 'static) {
		insert(&mut self.encoders, "encoder", name, Box::new(encoder));
	}
	pub fn encoder(&self, name: &str) -> Option<&dyn Encoder> {
		self.encoders.get(name).map(|b| b.as_ref())
	}
	pub fn register_value(&mut self, name: &str, model: impl ValueModel + 'static) {
		insert(&mut self.value, "value model", name, Box::new(model));
	}
	pub fn value_model(&self, name: &str) -> Option<&dyn ValueModel> {
		self.value.get(name).map(|b| b.as_ref())
	}
	pub fn register_asthir(&mut self, name: &str, specializer: impl AstHir + 'static) {
		insert(&mut self.asthir, "specializer", name, Box::new(specializer));
	}
	pub fn asthir(&self, name: &str) -> Option<&dyn AstHir> {
		self.asthir.get(name).map(|b| b.as_ref())
	}
	pub fn register_layout(&mut self, name: &str, layout: impl NativeLayout + 'static) {
		insert(&mut self.layout, "layout", name, Box::new(layout));
	}
	pub fn layout(&self, name: &str) -> Option<&dyn NativeLayout> {
		self.layout.get(name).map(|b| b.as_ref())
	}
	pub fn register_codegen<T: LanguageTokens>(
		&mut self,
		name: &str,
		codegen: impl NativeCodegen<T> + 'static,
	) {
		insert(T::storage_mut(self), "codegen", name, Box::new(codegen));
	}
	pub fn codegen<T: LanguageTokens>(&self, name: &str) -> Option<&dyn NativeCodegen<T>> {
		T::storage(self).get(name).map(|b| b.as_ref())
	}
	pub fn specializers(&self) -> BTreeSet<&str> {
		self.asthir
			.keys()
			.chain(self.layout.keys())
			.chain(self.codegen_rust.keys())
			.chain(self.codegen_ts.keys())
			.chain(self.value.keys())
			.map(String::as_str)
			.collect()
	}
}

/// A def param type for generated Args structs: uninferred (const-only usage) params
/// fall back to a plain integer.
pub fn param_ty(ctx: &NativeCodegenCtx<Rust>, id: TypeId) -> rust::Tokens {
	if id.is_valid() {
		quote!($(ctx.names.ty(id)))
	} else {
		quote!(usize)
	}
}

/// the `crate::pds` IO types every generated module uses
pub fn pds_io() -> (rust::Import, rust::Import, rust::Import) {
	(
		rust::import("crate::pds", "Input"),
		rust::import("crate::pds", "Output"),
		rust::import("crate::pds", "Result"),
	)
}

pub fn list_value(
	cx: &Elaborator,
	id: TypeId,
	native: &mir::NativeType,
	value: &mir::Expr,
) -> Result<mir::Construction> {
	let mir::Expr::List(items) = value else {
		bail!("{} is written as a list `[...]`", cx.name(id));
	};
	let elem = specialization_use(native, VALUE)
		.ok_or_else(|| anyhow!("list without a value type"))?
		.target;
	let items = items
		.iter()
		.enumerate()
		.map(|(i, e)| cx.coerce(e.clone(), elem).with_context(|| format!("item {i}")))
		.collect::<Result<_>>()?;
	Ok(mir::Construction::List(items))
}

pub fn list_items<'c, L: Lang>(cx: &LowerCx<L>, c: &'c mir::Construction) -> Result<Vec<Tokens<L>>> {
	let mir::Construction::List(items) = c else {
		bail!("list is constructed from items, got {c:?}");
	};
	items.iter().map(|e| (cx.lower)(e)).collect()
}

pub fn construct_vec(cx: &LowerCx<Rust>, c: &mir::Construction) -> Result<rust::Tokens> {
	let items = list_items(cx, c)?;
	Ok(quote!(vec![$(for i in items join(, ) => $i)]))
}
