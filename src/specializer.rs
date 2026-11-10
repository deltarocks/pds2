//! Specializers: Rust implementations that turn a use-site body of a native type
//! into a concrete list of typed fields.
//!
//! A `def_native` declares `body;` to accept a body and `@specializer name` to pick
//! the implementation. The `apply_specializers` pass calls [`Specializer::fields`]
//! during HIR desugaring (after path resolution) to project the body into fields.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{rust, JavaScript, Lang, Rust};
use genco::prelude::*;
use genco::Tokens;
use tracing::info;

pub mod container;
mod typescript;
pub mod union;

use container::Container;

use crate::ast::{self, Ident, Statement};
use crate::generator::common::EqKind;
use crate::upgrade::{keys_fit, plan, NativeSig, NativeSigVariant, TagKind, UpgradeCtx};

use crate::hir::{self, HirSpecializerCtx, TypeId};
use crate::mir;

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
}
impl Scalar {
	pub fn parse(s: &str) -> Option<Self> {
		Some(match s {
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
	/// types this native is compared against when used as a union discriminant; a
	/// tag-capable native (e.g. `option`) emits an `is_equal_{t}` helper per entry.
	pub eq_targets: &'a [(TypeId, EqKind)],
	pub types: &'a mir::TypeMap,
	pub virtual_used: &'a dyn Fn(&str) -> bool,
	pub read_ref: &'a dyn Fn(&mir::Field, bool) -> Tokens<L>,
	pub read_arg: &'a dyn Fn(&mir::FieldOrConst, bool) -> Tokens<L>,
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
}

const VALUE: &str = "Value";

fn specialization_use<'n>(native: &'n mir::NativeType, name: &str) -> Option<&'n mir::Type> {
	native
		.specialization
		.iter()
		.find(|(n, _)| n.as_str() == name)
		.map(|(_, t)| t)
}

fn visit_specialization(
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

fn visit_fields(native: &mir::NativeType, embed: Embed, v: &mut dyn SlotVisitor) {
	for (n, t) in &native.fields {
		v.slot(n, t.target, embed);
	}
}

fn vec_of_value(native: &mir::NativeType, names: &dyn Names<Rust>) -> Option<rust::Tokens> {
	let value = specialization_use(native, VALUE)?.target;
	Some(quote!(Vec<$(names.ty(value))>))
}

pub trait Upgrade {
	fn name(&self) -> String;
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: None,
			..Default::default()
		}
	}
	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ctx.upgrade_type_generic(&self.name(), data)
	}
}

#[derive(Default)]
pub struct Registry {
	pub(crate) asthir: HashMap<String, Box<dyn AstHir>>,
	pub(crate) upgrade: HashMap<String, Box<dyn Upgrade>>,
	pub(crate) layout: HashMap<String, Box<dyn NativeLayout>>,
	pub(crate) codegen_rust: HashMap<String, Box<dyn NativeCodegen<Rust>>>,
	pub(crate) codegen_ts: HashMap<String, Box<dyn NativeCodegen<JavaScript>>>,
}

impl Registry {
	pub fn register_asthir(&mut self, name: &str, specializer: impl AstHir + 'static) {
		self.asthir.insert(name.to_owned(), Box::new(specializer));
	}
	pub fn register_layout(&mut self, name: &str, layout: impl NativeLayout + 'static) {
		self.layout.insert(name.to_owned(), Box::new(layout));
	}
	pub fn layout(&self, name: &str) -> Option<&dyn NativeLayout> {
		self.layout.get(name).map(|b| b.as_ref())
	}
	pub fn register_upgrade(&mut self, specializer: impl Upgrade + 'static) {
		let spec = Box::new(specializer);
		self.upgrade.insert(spec.name(), spec);
	}
	pub fn register_codegen<T: LanguageTokens>(
		&mut self,
		name: &str,
		codegen: impl NativeCodegen<T> + 'static,
	) {
		T::storage_mut(self).insert(name.to_owned(), Box::new(codegen));
	}
	pub fn codegen<T: LanguageTokens>(&self, name: &str) -> Option<&dyn NativeCodegen<T>> {
		T::storage(self).get(name).map(|b| b.as_ref())
	}
}

pub fn register_protodef_types(reg: &mut Registry) {
	reg.register_asthir("bitfield", Bitfield);
	reg.register_asthir("bitflags", Bitflags);
	reg.register_asthir("array", Array);
	reg.register_asthir("registryEntryHolder", RegistryEntryHolder);
	reg.register_asthir("registryEntryHolderSet", RegistryEntryHolderSet);
	reg.register_asthir("entityMetadataLoop", EntityMetadataLoop);
	reg.register_asthir("terminated", EntityMetadataLoop);
	reg.register_asthir("container", Container);
	reg.register_layout("container", Container);
	reg.register_codegen::<Rust>("container", Container);
	reg.register_codegen::<JavaScript>("container", Container);
	reg.register_asthir("union", union::Union);
	reg.register_layout("union", union::Union);
	reg.register_codegen::<Rust>("union", union::Union);
	reg.register_codegen::<JavaScript>("union", union::Union);
	reg.register_upgrade(union::Union);

	reg.register_layout("bitfield", Bitfield);
	reg.register_layout("bitflags", Bitflags);
	reg.register_layout("array", Array);
	reg.register_layout("option", OptionP);
	reg.register_layout("buffer", Buffer);
	reg.register_layout("pstring", PString);
	reg.register_layout("entityMetadataLoop", EntityMetadataLoop);
	reg.register_layout("terminated", EntityMetadataLoop);
	reg.register_layout("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	reg.register_layout("registryEntryHolder", RegistryEntryHolder);
	reg.register_layout("registryEntryHolderSet", RegistryEntryHolderSet);

	reg.register_codegen::<Rust>("bitfield", Bitfield);
	reg.register_codegen::<Rust>("bitflags", Bitflags);
	reg.register_codegen::<Rust>("array", Array);
	reg.register_codegen::<Rust>("option", OptionP);
	reg.register_codegen::<Rust>("buffer", Buffer);
	reg.register_codegen::<Rust>("pstring", PString);
	reg.register_codegen::<Rust>("entityMetadataLoop", EntityMetadataLoop);
	reg.register_codegen::<Rust>("terminated", EntityMetadataLoop);
	reg.register_codegen::<Rust>("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	reg.register_codegen::<Rust>("registryEntryHolder", RegistryEntryHolder);
	reg.register_codegen::<Rust>("registryEntryHolderSet", RegistryEntryHolderSet);

	typescript::register(reg);

	reg.register_upgrade(Bitfield);
	reg.register_upgrade(Bitflags);
	reg.register_upgrade(RegistryEntryHolder);
	reg.register_upgrade(RegistryEntryHolderSet);
	reg.register_upgrade(Array);
	reg.register_upgrade(Buffer);
	reg.register_upgrade(EntityMetadataLoop);
	reg.register_upgrade(OptionP);
	reg.register_upgrade(UUID);
	reg.register_upgrade(AnonOptionalNbt);
	reg.register_upgrade(AnonymousNbt);
	reg.register_upgrade(BoolP);
	reg.register_upgrade(Container);
	reg.register_upgrade(F32P);
	reg.register_upgrade(F64P);
	reg.register_upgrade(I8P);
	reg.register_upgrade(I16P);
	reg.register_upgrade(I32P);
	reg.register_upgrade(I64P);
	reg.register_upgrade(Varint);
	reg.register_upgrade(LpVec3);
	reg.register_upgrade(PString);
	reg.register_upgrade(RestBuffer);
	reg.register_upgrade(Switch);
	reg.register_upgrade(Mapper);
	reg.register_upgrade(U8P);
	reg.register_upgrade(U16P);
	reg.register_upgrade(U32P);
	reg.register_upgrade(U64P);
	reg.register_upgrade(Varlong);
	reg.register_upgrade(Void);
	reg.register_upgrade(TopBitSetTerminatedArray);
}

macro_rules! trivial_upgrade {
	($struct:ident => $name:literal) => {
		struct $struct;
		impl Upgrade for $struct {
			fn name(&self) -> String {
				$name.to_owned()
			}
		}
	};
	($struct:ident => $name:literal, scalar: $scalar:literal) => {
		struct $struct;
		impl Upgrade for $struct {
			fn name(&self) -> String {
				$name.to_owned()
			}
			fn sig(&self) -> NativeSig {
				NativeSig {
					scalar: Some($scalar.to_owned()),
					..Default::default()
				}
			}
		}
	};
}
struct Void;
impl Upgrade for Void {
	fn name(&self) -> String {
		"void".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			is_void: true,
			..Default::default()
		}
	}
}

trivial_upgrade!(U8P => "u8", scalar: "u8");
trivial_upgrade!(U16P => "u16", scalar: "u16");
trivial_upgrade!(U32P => "u32", scalar: "u32");
trivial_upgrade!(U64P => "u64", scalar: "u64");
trivial_upgrade!(I8P => "i8", scalar: "i8");
trivial_upgrade!(I16P => "i16", scalar: "i16");
trivial_upgrade!(I32P => "i32", scalar: "i32");
trivial_upgrade!(I64P => "i64", scalar: "i64");
trivial_upgrade!(F32P => "f32", scalar: "f32");
trivial_upgrade!(F64P => "f64", scalar: "f64");
trivial_upgrade!(Varint => "varint", scalar: "i32");
trivial_upgrade!(Varlong => "varlong", scalar: "i64");

struct TopBitSetTerminatedArray;
impl NativeLayout for TopBitSetTerminatedArray {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}
impl NativeCodegen<Rust> for TopBitSetTerminatedArray {
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		vec_of_value(native, names)
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("topBitSetTerminatedArray value")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("topBitSetTerminatedArray type")?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let perror = &rust::import("crate::pds", "Error");
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				if _0.is_empty() {
					return Err($perror::InvalidValue);
				}
				for (i, e) in _0.iter().enumerate() {
					let mut item = $poutput::new();
					$(ctx.names.serialize(value))(e, &mut item)?;
					let mut item = item.into_inner();
					let Some(first) = item.first_mut() else {
						return Err($perror::InvalidValue);
					};
					if *first & 0x80 != 0 {
						return Err($perror::InvalidValue);
					}
					if i + 1 < _0.len() {
						*first |= 0x80;
					}
					output.write(&item);
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let mut out = Vec::new();
				loop {
					let more = input.peek()? & 0x80 != 0;
					input.mask_next(0x7f);
					out.push($(ctx.deserialize_call(value_use))?);
					if !more {
						return Ok(out);
					}
				}
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = 0;
				for e in _0.iter() { n += $(ctx.names.size_of(value))(e); }
				n
			}
		})
	}
}
impl Upgrade for TopBitSetTerminatedArray {
	fn name(&self) -> String {
		"topBitSetTerminatedArray".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("topBitSetTerminatedArray".to_owned()),
			default: NativeSigVariant {
				value_generic: true,
				..Default::default()
			},
			..Default::default()
		}
	}
}

/// Packed named integers, MSB-first. Each field projects to the smallest native int
/// that fits its bit width.
struct Bitfield;

struct BitfieldEntry {
	name: Ident,
	size: u32,
	signed: bool,
	/// width of the smallest native int fitting `size`
	bits: u32,
}
impl BitfieldEntry {
	/// the primitive the entry projects to (`i32`, `u8`, ...)
	fn prim(&self) -> String {
		format!("{}{}", if self.signed { "i" } else { "u" }, self.bits)
	}
}
fn bitfield_entry(name: &Ident, size: Option<i128>, signed: Option<bool>) -> Result<BitfieldEntry> {
	let size = match size {
		Some(n) if n > 0 => n,
		_ => bail!("bitfield field {name:?} needs a positive integer `size`"),
	};
	let Some(signed) = signed else {
		bail!("bitfield field {name:?}: `signed` should be a boolean");
	};
	let bits = match size {
		0..=8 => 8,
		9..=16 => 16,
		17..=32 => 32,
		33..=64 => 64,
		_ => bail!("bitfield field {name:?} larger than 64 bits"),
	};
	Ok(BitfieldEntry {
		name: name.clone(),
		size: size as u32,
		signed,
		bits,
	})
}

fn bitfield_entries(body: &[mir::BodyItem]) -> Result<Vec<BitfieldEntry>> {
	body.iter()
		.map(|f| {
			let size = match f.constant("size") {
				Some(mir::Value::Signed(n)) => Some(*n),
				Some(mir::Value::Unsigned(n)) => i128::try_from(*n).ok(),
				_ => None,
			};
			let signed = match f.constant("signed") {
				Some(mir::Value::Bool(b)) => Some(*b),
				_ => None,
			};
			bitfield_entry(f.expect_name()?, size, signed)
		})
		.collect()
}

impl AstHir for Bitfield {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ctx.body
			.iter()
			.map(|f| {
				let size = match f.constant("size") {
					Some(hir::Value::Number(n)) => Some(*n),
					_ => None,
				};
				let signed = match f.constant("signed") {
					Some(hir::Value::Bool(b)) => Some(*b),
					_ => None,
				};
				let e = bitfield_entry(f.expect_name()?, size, signed)?;
				Ok((e.name.clone(), ctx.native(&e.prim())?))
			})
			.collect()
	}
}

impl NativeLayout for Bitfield {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_fields(native, Embed::Fixed, v);
	}
}
impl NativeCodegen<Rust> for Bitfield {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let entries = bitfield_entries(&ctx.native.body)?;
		let total: u32 = entries.iter().map(|e| e.size).sum();
		ensure!(
			total % 8 == 0,
			"bitfield does not pack into whole bytes: {total} bits"
		);
		let bytes = total / 8;
		let u8id = (ctx.resolve)("u8").ok_or_else(|| anyhow!("bitfield needs the `u8` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);

		let mut extract = Vec::new();
		let mut offset = 0;
		for e in &entries {
			let shift = total - offset - e.size;
			let mask = format!("{:#x}", (1u128 << e.size) - 1);
			let prim = e.prim();
			extract.push(if e.signed {
				// place the value at the top of the int and arithmetic-shift back down
				// to sign-extend
				let pad = e.bits - e.size;
				quote! {
					let $(&e.name) = (((((acc >> $(shift.to_string())) & $(&mask)) as u$(e.bits.to_string())) << $(pad.to_string())) as $(&prim)) >> $(pad.to_string());
				}
			} else {
				quote! {
					let $(&e.name) = ((acc >> $(shift.to_string())) & $(&mask)) as $(&prim);
				}
			});
			offset += e.size;
		}

		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub struct Ty {
				$(for (e, (_, ty)) in entries.iter().zip(ctx.native.fields.iter()) join($['\r']) =>
					pub $(&e.name): $(ctx.names.ty(ty.target)),
				)
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				let mut acc: u128 = 0;
				$(for e in &entries join($['\r']) =>
					acc = (acc << $(e.size.to_string())) | ((_0.$(&e.name) as u128) & $(format!("{:#x}", (1u128 << e.size) - 1)));
				)
				for i in (0..$(bytes.to_string())u32).rev() {
					$(ctx.names.serialize(u8id))(&(((acc >> (i * 8)) & 0xff) as u8), output)?;
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let mut acc: u128 = 0;
				for _ in 0..$(bytes.to_string())u32 {
					acc = (acc << 8) | ($(ctx.names.deserialize(u8id))(input)? as u128);
				}
				$(for t in extract join($['\r']) => $t)
				Ok(Ty {
					$(for e in &entries join(,$[' ']) => $(&e.name))
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(bytes.to_string())
			}
		})
	}
}

/// A def param type for generated Args structs: uninferred (const-only usage) params
/// fall back to a plain integer.
fn param_ty(ctx: &NativeCodegenCtx<Rust>, id: TypeId) -> rust::Tokens {
	if id.is_valid() {
		quote!($(ctx.names.ty(id)))
	} else {
		quote!(usize)
	}
}

/// the `crate::pds` IO types every generated module uses
fn pds_io() -> (rust::Import, rust::Import, rust::Import) {
	(
		rust::import("crate::pds", "Input"),
		rust::import("crate::pds", "Output"),
		rust::import("crate::pds", "Result"),
	)
}
impl Upgrade for Bitfield {
	fn name(&self) -> String {
		"bitfield".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("bitfield".to_owned()),
			default: NativeSigVariant {
				accepts_body: Some("item(field) { argument(size); argument(signed) => false; };"),
				..Default::default()
			},
			..Default::default()
		}
	}

	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(data.len() == 1, "bitfield should have exactly one argument");
		let data = data[0]
			.as_array()
			.ok_or_else(|| anyhow!("bitfield argument should be array"))?;
		let mut statements = Vec::new();
		for item in data {
			let item = item
				.as_object()
				.ok_or_else(|| anyhow!("bitfield entry should be object"))?;
			let name = item
				.get("name")
				.and_then(serde_json::Value::as_str)
				.ok_or_else(|| anyhow!("bitfield entry missing name"))?;
			let size = item
				.get("size")
				.ok_or_else(|| anyhow!("bitfield entry missing size"))?;
			let signed = item
				.get("signed")
				.and_then(serde_json::Value::as_bool)
				.unwrap_or(false);
			statements.push(Statement::simple(
				ast::Item::simple_builder("field")
					.argsb(
						ast::ItemArgs::builder()
							.arg(ast::Value::new_simple_item(name))
							.named("size", ctx.upgrade_value(size)?)
							.named(
								"signed",
								ast::Value::new_simple_item(if signed { "true" } else { "false" }),
							),
					)
					.build_values(),
			));
		}
		Ok(ast::Item::simple_builder("bitfield")
			.block(ast::Block {
				attributes: Default::default(),
				statements,
				root: false,
			})
			.build_values())
	}
}

/// A `Value`-typed integer projected to a set of named booleans, one per flag bit.
struct Bitflags;
impl AstHir for Bitflags {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		let bool_ty = ctx.native("bool")?;
		ctx.body
			.iter()
			.map(|f| Ok((f.expect_name()?.clone(), bool_ty.clone())))
			.collect()
	}
}

impl NativeLayout for Bitflags {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_fields(native, Embed::Fixed, v);
	}
}
impl NativeCodegen<Rust> for Bitflags {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value = ctx.specialization("Value")?;
		// flag bit i = 1 << i, in declaration order
		let bit = |i: usize| format!("{:#x}", 1u64 << i);
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);

		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub struct Ty {
				$(for (name, ty) in ctx.native.fields.iter() join($['\r']) =>
					pub $name: $(ctx.names.ty(ty.target)),
				)
			}
			fn pack(_0: &Ty) -> u64 {
				let mut acc: u64 = 0;
				$(for (i, (name, _)) in ctx.native.fields.iter().enumerate() join($['\r']) =>
					if _0.$name { acc |= $(bit(i)); }
				)
				acc
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(ctx.names.serialize(value))(&(pack(_0) as $(ctx.names.ty(value))), output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let acc = $(ctx.names.deserialize(value))(input)? as u64;
				Ok(Ty {
					$(for (i, (name, _)) in ctx.native.fields.iter().enumerate() join($['\r']) =>
						$name: acc & $(bit(i)) != 0,
					)
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(ctx.names.size_of(value))(&(pack(_0) as $(ctx.names.ty(value))))
			}
		})
	}
}
impl Upgrade for Bitflags {
	fn name(&self) -> String {
		"bitflags".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("bitflags".to_owned()),
			default: NativeSigVariant {
				accepts_body: Some("item(flag);"),
				value_generic: true,
				..Default::default()
			},
			..Default::default()
		}
	}

	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(data.len() == 1, "bitflags should have exactly one argument");
		let obj = data[0]
			.as_object()
			.ok_or_else(|| anyhow!("bitflags needs object"))?;

		let ty = obj
			.get("type")
			.ok_or_else(|| anyhow!("bitflags missing type"))?;
		let flags = obj
			.get("flags")
			.and_then(serde_json::Value::as_array)
			.ok_or_else(|| anyhow!("bitflags missing flags"))?;
		let mut statements = Vec::new();
		for flag in flags {
			let flag = flag
				.as_str()
				.ok_or_else(|| anyhow!("flag should be string"))?;
			statements.push(Statement::simple(
				ast::Item::simple_builder("flag")
					.argsb(ast::ItemArgs::builder().arg(ast::Value::new_simple_item(flag)))
					.build_values(),
			));
		}
		Ok(ast::Item::simple_builder("bitflags")
			.block(ast::Block {
				attributes: Default::default(),
				statements,
				root: false,
			})
			.then(
				ctx.wrapped(|ctx| ctx.upgrade_type(ty.clone()))
					.context("bitflags value")?,
			))
	}
}

fn upgrade_registry_holder(
	name: &str,
	obj: &serde_json::Value,
	ctx: &mut UpgradeCtx,
) -> Result<Vec<ast::Value>> {
	let obj = obj
		.as_object()
		.ok_or_else(|| anyhow!("{name} needs an object"))?;
	let mut args = ast::ItemArgs::builder();
	if let Some(base_name) = obj.get("baseName").and_then(serde_json::Value::as_str) {
		args = args.named("baseName", ast::Value::new_string(base_name.to_owned()));
	}
	let mut statements = Vec::new();
	if let Some(base) = obj.get("base").and_then(serde_json::Value::as_object) {
		statements.push(ctx.upgrade_named_field(base)?);
	}
	if let Some(otherwise) = obj.get("otherwise").and_then(serde_json::Value::as_object) {
		statements.push(ctx.upgrade_named_field(otherwise)?);
	}
	Ok(ast::Item::simple_builder(name)
		.args(args.build())
		.block(ast::Block {
			attributes: Default::default(),
			statements,
			root: false,
		})
		.build_values())
}
/// Project a use-site body of `field(name) => type` statements into typed fields.
fn body_fields(ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
	let mut out = Vec::new();
	for f in ctx.body {
		ensure!(
			f.kind.as_str() == "field",
			"unexpected body statement {:?}",
			f.kind
		);
		let name = f.expect_name()?;
		let ty =
			f.ty.clone()
				.ok_or_else(|| anyhow!("body field {name:?} needs a type"))?;
		out.push((name.clone(), ty));
	}
	Ok(out)
}

struct RegistryEntryHolder;
impl AstHir for RegistryEntryHolder {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		body_fields(ctx)
	}
}
impl NativeLayout for RegistryEntryHolder {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		if let Some((name, ty)) = native.fields.first() {
			v.slot(name, ty.target, Embed::Boxable(BoxCost::Low));
		}
	}
}
impl NativeCodegen<Rust> for RegistryEntryHolder {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolder needs the `varint` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		// the projected use-site body field is the inline (id 0) representation
		let Some((iname, ity_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolder needs an inline field from its body");
		};
		let ity = ity_use.target;
		let read_inline = ctx.slot_new(iname, quote!($(ctx.deserialize_call(ity_use))?));
		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub enum Ty {
				Id($(ctx.names.ty(varint))),
				Inline($(ctx.slot_ty(iname, ity))),
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				match _0 {
					Ty::Id(id) => $(ctx.names.serialize(varint))(&(id + 1), output),
					Ty::Inline(v) => {
						$(ctx.names.serialize(varint))(&0, output)?;
						$(ctx.names.serialize(ity))(v, output)
					}
				}
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let n = $(ctx.names.deserialize(varint))(input)?;
				Ok(if n == 0 {
					Ty::Inline($read_inline)
				} else {
					Ty::Id(n - 1)
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				match _0 {
					Ty::Id(id) => $(ctx.names.size_of(varint))(&(id + 1)),
					Ty::Inline(v) => $(ctx.names.size_of(varint))(&0) + $(ctx.names.size_of(ity))(v),
				}
			}
		})
	}
}
impl Upgrade for RegistryEntryHolder {
	fn name(&self) -> String {
		"registryEntryHolder".to_owned()
	}

	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("registryEntryHolder".to_owned()),
			default: NativeSigVariant {
				args: BTreeSet::from_iter(["baseName".to_owned()]),
				accepts_body: Some("item(field) { type(Value); };"),
				..Default::default()
			},
			..Default::default()
		}
	}

	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(
			data.len() == 1,
			"registryEntryHolder should have exactly one argument"
		);
		upgrade_registry_holder("registryEntryHolder", &data[0], ctx)
	}
}
struct RegistryEntryHolderSet;
impl AstHir for RegistryEntryHolderSet {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		body_fields(ctx)
	}
}
impl NativeLayout for RegistryEntryHolderSet {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		if let Some((name, ty)) = native.fields.first() {
			v.slot(name, ty.target, Embed::Boxable(BoxCost::Low));
		}
	}
}
impl NativeCodegen<Rust> for RegistryEntryHolderSet {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolderSet needs the `varint` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let Some((nname, nty_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolderSet needs a named field from its body");
		};
		let nty = nty_use.target;
		let read_named = ctx.slot_new(nname, quote!($(ctx.deserialize_call(nty_use))?));
		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub enum Ty {
				Named($(ctx.slot_ty(nname, nty))),
				Ids(Vec<$(ctx.names.ty(varint))>),
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				match _0 {
					Ty::Named(v) => {
						$(ctx.names.serialize(varint))(&0, output)?;
						$(ctx.names.serialize(nty))(v, output)
					}
					Ty::Ids(ids) => {
						$(ctx.names.serialize(varint))(&(ids.len() + 1).try_into().map_err(|_| $(rust::import("crate::pds", "Error"))::InvalidValue)?, output)?;
						for id in ids.iter() {
							$(ctx.names.serialize(varint))(id, output)?;
						}
						Ok(())
					}
				}
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let n: usize = $(ctx.names.deserialize(varint))(input)?.try_into().map_err(|_| $(rust::import("crate::pds", "Error"))::InvalidValue)?;
				Ok(if n == 0 {
					Ty::Named($read_named)
				} else {
					let mut ids = Vec::with_capacity((n - 1).min(input.remaining()));
					for _ in 0..n - 1 {
						ids.push($(ctx.names.deserialize(varint))(input)?);
					}
					Ty::Ids(ids)
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				match _0 {
					Ty::Named(v) => 1 + $(ctx.names.size_of(nty))(v),
					Ty::Ids(ids) => {
						let mut n = $(ctx.names.size_of(varint))(&(ids.len() + 1).try_into().unwrap_or_default());
						for id in ids.iter() { n += $(ctx.names.size_of(varint))(id); }
						n
					}
				}
			}
		})
	}
}
impl Upgrade for RegistryEntryHolderSet {
	fn name(&self) -> String {
		"registryEntryHolderSet".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("registryEntryHolderSet".to_owned()),
			default: NativeSigVariant {
				args: BTreeSet::from_iter(["baseName".to_owned()]),
				accepts_body: Some("item(field) { type(Value); };"),
				..Default::default()
			},
			..Default::default()
		}
	}

	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(
			data.len() == 1,
			"registryEntryHolderSet should have exactly one argument"
		);
		upgrade_registry_holder("registryEntryHolderSet", &data[0], ctx)
	}
}

struct Array;
impl NativeLayout for Array {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}
impl NativeCodegen<Rust> for Array {
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		vec_of_value(native, names)
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("array element type")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("array type")?;
		let de_value = |ctx: &NativeCodegenCtx<Rust>| ctx.deserialize_call(value_use);
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let perror = &rust::import("crate::pds", "Error");

		let main = match ctx.variant.as_str() {
			// length prefix read inline, as the `Count` type
			"inline" => {
				let count = ctx.specialization("Count").context("array count type")?;
				quote! {
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						$(ctx.names.serialize(count))(&_0.len().try_into().map_err(|_| $perror::InvalidValue)?, output)?;
						for e in _0.iter() {
							$(ctx.names.serialize(value))(e, output)?;
						}
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
						let count: usize = $(ctx.names.deserialize(count))(input)?.try_into().map_err(|_| $perror::InvalidValue)?;
						let mut out = Vec::with_capacity(count.min(input.remaining()));
						for _ in 0..count {
							out.push($(de_value(ctx))?);
						}
						Ok(out)
					}
					pub fn size_of(_0: &Ty) -> usize {
						let mut n = $(ctx.names.size_of(count))(&_0.len().try_into().unwrap_or_default());
						for e in _0.iter() { n += $(ctx.names.size_of(value))(e); }
						n
					}
				}
			}
			// length supplied by the use-site (field reference or constant)
			"sized" => {
				ensure!(
					!ctx.params.is_empty(),
					"array::sized needs a `count` argument"
				);
				quote! {
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						for e in _0.iter() {
							$(ctx.names.serialize(value))(e, output)?;
						}
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
						let count: usize = (*args.count).try_into().map_err(|_| $perror::InvalidValue)?;
						let mut out = Vec::with_capacity(count.min(input.remaining()));
						for _ in 0..count {
							out.push($(de_value(ctx))?);
						}
						Ok(out)
					}
					pub fn size_of(_0: &Ty) -> usize {
						let mut n = 0;
						for e in _0.iter() { n += $(ctx.names.size_of(value))(e); }
						n
					}
				}
			}
			v => bail!("unknown array overload: {v:?}"),
		};

		// the runtime length, referencable as `@size` (e.g. by a virtual length field)
		let virtual_size = match ctx.virtual_field("size") {
			Some(virt) => quote! {
				pub fn virtual_size(_0: &Ty) -> $(ctx.names.ty(virt)) {
					_0.len().try_into().expect("array length")
				}
			},
			None => rust::Tokens::new(),
		};

		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			$main
			$virtual_size
		})
	}
}
impl AstHir for Array {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ensure!(
			ctx.body.is_empty(),
			"array does not accept a body, got {:?}",
			ctx.body
		);
		// Validate the overload's inputs; the array itself is opaque (no named
		// fields): its element/count bindings live in the specialization and the
		// wire-time `size` argument stays on the native instance.
		match ctx.variant.as_str() {
			// length prefix read inline, as the `Size` type
			"inline" => ensure!(
				ctx.types.contains_key(&Ident::new("Count")),
				"array::inline needs a `Count:` type binding"
			),
			// length supplied by the use-site (field reference or constant)
			"sized" => ensure!(
				ctx.args.iter().any(|(n, _)| n.as_str() == "count"),
				"array::sized needs a `count:` argument"
			),
			v => bail!("unknown array overload: {v:?}"),
		}
		Ok(vec![])
	}
}
impl Upgrade for Array {
	fn name(&self) -> String {
		"array".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("array".to_owned()),
			is_void: false,
			scalar: None,
			default: NativeSigVariant {
				value_generic: true,
				..Default::default()
			},
			variants: BTreeMap::from_iter([
				(
					"inline".to_owned(),
					NativeSigVariant {
						generics: BTreeSet::from_iter(["Count".to_owned()]),
						..Default::default()
					},
				),
				(
					"sized".to_owned(),
					NativeSigVariant {
						args: BTreeSet::from_iter(["count".to_owned()]),
						..Default::default()
					},
				),
			]),
		}
	}
}
struct Buffer;
impl NativeLayout for Buffer {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}
impl NativeCodegen<Rust> for Buffer {
	fn inline_ty(
		&self,
		_id: TypeId,
		_native: &mir::NativeType,
		_names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		Some(quote!(Vec<u8>))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("buffer type")?;
		let u8id = (ctx.resolve)("u8").ok_or_else(|| anyhow!("buffer needs the `u8` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let perror = &rust::import("crate::pds", "Error");
		let read_bytes = quote! {
			if count > input.remaining() {
				return Err($perror::NotEnoughData);
			}
			let mut out = Vec::with_capacity(count);
			for _ in 0..count {
				out.push($(ctx.names.deserialize(u8id))(input)?);
			}
			Ok(out)
		};
		let write_bytes = quote! {
			for b in _0.iter() {
				$(ctx.names.serialize(u8id))(b, output)?;
			}
		};
		let main = match ctx.variant.as_str() {
			"inline" => {
				let count = ctx.specialization("Count").context("buffer count type")?;
				quote! {
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						$(ctx.names.serialize(count))(&_0.len().try_into().map_err(|_| $perror::InvalidValue)?, output)?;
						$write_bytes
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
						let count: usize = $(ctx.names.deserialize(count))(input)?.try_into().map_err(|_| $perror::InvalidValue)?;
						$read_bytes
					}
					pub fn size_of(_0: &Ty) -> usize {
						$(ctx.names.size_of(count))(&_0.len().try_into().unwrap_or_default()) + _0.len()
					}
				}
			}
			"sized" => {
				quote! {
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						$write_bytes
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
						let count: usize = (*args.count).try_into().map_err(|_| $perror::InvalidValue)?;
						$read_bytes
					}
					pub fn size_of(_0: &Ty) -> usize {
						_0.len()
					}
				}
			}
			v => bail!("unknown buffer overload: {v:?}"),
		};
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			$main
		})
	}
}
impl Upgrade for Buffer {
	fn name(&self) -> String {
		"buffer".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("buffer".to_owned()),
			variants: BTreeMap::from_iter([
				(
					"inline".to_owned(),
					NativeSigVariant {
						generics: BTreeSet::from_iter(["Count".to_owned()]),
						..Default::default()
					},
				),
				(
					"sized".to_owned(),
					NativeSigVariant {
						args: BTreeSet::from_iter(["count".to_owned()]),
						..Default::default()
					},
				),
			]),
			..Default::default()
		}
	}
}

struct EntityMetadataLoop;
impl NativeLayout for EntityMetadataLoop {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}
impl NativeCodegen<Rust> for EntityMetadataLoop {
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		vec_of_value(native, names)
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("terminated array value")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("terminated array type")?;
		let end = match ctx
			.native
			.const_args
			.iter()
			.find(|(n, _)| n.as_str() == "end")
		{
			Some((_, hir::Value::Number(n))) => {
				u8::try_from(*n).map_err(|_| anyhow!("terminator {n} does not fit a byte"))?
			}
			other => bail!("terminated array needs a constant byte `end`, got {other:?}"),
		};
		let end = end.to_string();
		let u8id =
			(ctx.resolve)("u8").ok_or_else(|| anyhow!("terminated array needs the `u8` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				for e in _0.iter() {
					$(ctx.names.serialize(value))(e, output)?;
				}
				$(ctx.names.serialize(u8id))(&$(&end), output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let mut out = Vec::new();
				loop {
					if input.peek()? == $(&end) {
						input.skip();
						return Ok(out);
					}
					out.push($(ctx.deserialize_call(value_use))?);
				}
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = 1;
				for e in _0.iter() { n += $(ctx.names.size_of(value))(e); }
				n
			}
		})
	}
}
impl AstHir for EntityMetadataLoop {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		ensure!(
			ctx.body.is_empty(),
			"terminated array does not accept a body, got {:?}",
			ctx.body
		);
		ensure!(
			ctx.args.iter().any(|(n, _)| n.as_str() == "end"),
			"terminated array needs an `end:` argument"
		);
		Ok(vec![])
	}
}
impl Upgrade for EntityMetadataLoop {
	fn name(&self) -> String {
		"entityMetadataLoop".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("entityMetadataLoop".to_owned()),
			default: NativeSigVariant {
				value_generic: true,
				accepts_body: Some(""),
				args: BTreeSet::from_iter(["end".to_owned()]),
				..Default::default()
			},
			..Default::default()
		}
	}
}
struct OptionP;
impl NativeLayout for OptionP {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Boxable(BoxCost::Low), v);
	}
}
impl NativeCodegen<Rust> for OptionP {
	fn inline_ty(
		&self,
		id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		let value = specialization_use(native, VALUE)?.target;
		Some(quote!(Option<$(names.slot_ty(id, &Ident::new(VALUE), value))>))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("option value type")?;
		let value = value_use.target;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("option type")?;
		let value_slot = Ident::new(VALUE);
		let read_value = ctx.slot_new(&value_slot, quote!($(ctx.deserialize_call(value_use))?));
		let as_ref = if ctx.boxed(&value_slot) {
			quote!(as_deref)
		} else {
			quote!(as_ref)
		};
		let boolean =
			(ctx.resolve)("bool").ok_or_else(|| anyhow!("option needs the `bool` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(ctx.names.serialize(boolean))(&_0.is_some(), output)?;
				if let Some(v) = _0 {
					$(ctx.names.serialize(value))(v, output)?;
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				Ok(if $(ctx.names.deserialize(boolean))(input)? {
					Some($read_value)
				} else {
					None
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = $(ctx.names.size_of(boolean))(&_0.is_some());
				if let Some(v) = _0 { n += $(ctx.names.size_of(value))(v); }
				n
			}
			// Union arm matching when an `option` is used as a discriminant: each
			// target is the inner value type; the present value is compared to it.
			$(for (t, _) in ctx.eq_targets.iter().filter(|(_, k)| *k != EqKind::Bool) join($['\r']) =>
				pub fn $(format!("is_equal_{}", t.raw()))(a: &Ty, b: &$(ctx.names.ty(*t))) -> bool {
					a.$(&as_ref)() == Some(b)
				}
			)
		})
	}
}
impl Upgrade for OptionP {
	fn name(&self) -> String {
		"option".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("option".to_owned()),
			default: NativeSigVariant {
				value_generic: true,
				..Default::default()
			},
			..Default::default()
		}
	}
}

struct UUID;
impl Upgrade for UUID {
	fn name(&self) -> String {
		"UUID".to_owned()
	}
}
struct AnonOptionalNbt;
impl Upgrade for AnonOptionalNbt {
	fn name(&self) -> String {
		"anonOptionalNbt".to_owned()
	}
}
struct AnonymousNbt;
impl Upgrade for AnonymousNbt {
	fn name(&self) -> String {
		"anonymousNbt".to_owned()
	}
}
struct BoolP;
impl Upgrade for BoolP {
	fn name(&self) -> String {
		"bool".to_owned()
	}
}
struct LpVec3;
impl Upgrade for LpVec3 {
	fn name(&self) -> String {
		"lpVec3".to_owned()
	}
}
struct PString;
impl NativeLayout for PString {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}
impl NativeCodegen<Rust> for PString {
	fn inline_ty(
		&self,
		_id: TypeId,
		_native: &mir::NativeType,
		_names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		Some(quote!(String))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let count = ctx.specialization("Count").context("pstring count type")?;
		let ty = self
			.inline_ty(ctx.id, ctx.native, ctx.names)
			.context("pstring type")?;
		let u8id = (ctx.resolve)("u8").ok_or_else(|| anyhow!("pstring needs the `u8` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let perror = &rust::import("crate::pds", "Error");
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = $ty;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(ctx.names.serialize(count))(&_0.len().try_into().map_err(|_| $perror::InvalidValue)?, output)?;
				for b in _0.as_bytes() {
					$(ctx.names.serialize(u8id))(b, output)?;
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let count: usize = $(ctx.names.deserialize(count))(input)?.try_into().map_err(|_| $perror::InvalidValue)?;
				if count > input.remaining() {
					return Err($perror::NotEnoughData);
				}
				let mut out = Vec::with_capacity(count);
				for _ in 0..count {
					out.push($(ctx.names.deserialize(u8id))(input)?);
				}
				String::from_utf8(out).map_err(|_| $perror::InvalidValue)
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(ctx.names.size_of(count))(&_0.len().try_into().unwrap_or_default()) + _0.len()
			}
		})
	}
}
impl Upgrade for PString {
	fn name(&self) -> String {
		"pstring".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("pstring".to_owned()),
			default: NativeSigVariant {
				generics: BTreeSet::from_iter(["Count".to_owned()]),
				..Default::default()
			},
			..Default::default()
		}
	}
}
struct RestBuffer;
impl Upgrade for RestBuffer {
	fn name(&self) -> String {
		"restBuffer".to_owned()
	}
}
impl Upgrade for Container {
	fn name(&self) -> String {
		"container".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("container".to_owned()),
			default: NativeSigVariant {
				args: BTreeSet::new(),
				generics: BTreeSet::new(),
				value_generic: false,
				accepts_body: Some(
					"item(field) { type(Value); }; item(virtual_field) { argument(value); type(Value); };",
				),
			},
			..Default::default()
		}
	}
	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(data.len() == 1, "container should have argument");
		let data = data[0]
			.as_array()
			.ok_or_else(|| anyhow!("expected container data to be array"))?;
		let fields = plan::inline_fields(data);
		let scope = plan::scope_names(&fields);
		let names: BTreeSet<String> = fields.iter().map(|(n, _)| n.clone()).collect();

		// protodef `mapper` (discriminant) + `switch(compareTo: mapper)` is a pds
		// inline-tag union; merge them when the mapper feeds exactly one switch and
		// nothing else refers to it.
		type JsonMap = serde_json::Map<String, serde_json::Value>;
		enum Plan {
			Keep,
			Absorbed,
			Inline(TagKind, JsonMap, serde_json::Value, String),
			External(JsonMap),
			Virtual(String),
		}
		let mut plans: Vec<Plan> = fields.iter().map(|_| Plan::Keep).collect();
		for (i, (tag, tag_field)) in fields.iter().enumerate() {
			if tag_field.get("name").is_none() || !matches!(plans[i], Plan::Keep) {
				continue;
			}
			let refs = plan::count_refs(&fields, tag);
			if refs == 0 {
				continue;
			}
			let switch_of = |j: usize| -> Option<&JsonMap> {
				let ty = fields[j].1.get("type")?;
				(plan::head(ty)? == "switch").then_some(())?;
				plan::arg(ty)?.as_object()
			};
			let keyed = |j: usize| {
				switch_of(j)
					.and_then(|s| s.get("compareTo"))
					.and_then(serde_json::Value::as_str)
					== Some(tag.as_str())
			};
			let run: Vec<usize> = (i + 1..fields.len()).take_while(|&j| keyed(j)).collect();
			if run.len() >= 2 {
				if refs != run.len() || run.iter().any(|&j| !matches!(plans[j], Plan::Keep)) {
					continue;
				}
				let switches: Vec<&serde_json::Value> = run.iter().map(|&j| &fields[j].1).collect();
				let Some(product) = plan::product(tag, &switches, &scope) else {
					continue;
				};
				let Some(tag_ty) = tag_field.get("type") else {
					continue;
				};
				let Some(kind) = ctx.classify_tag(tag_ty) else {
					continue;
				};
				if matches!(kind, TagKind::OptionScalar) || !keys_fit(&kind, &product) {
					continue;
				}
				plans[i] = Plan::Absorbed;
				for &j in &run[1..] {
					plans[j] = Plan::Absorbed;
				}
				plans[run[0]] = Plan::Inline(kind, product, tag_ty.clone(), tag.clone());
				continue;
			}
			if refs > 2 {
				continue;
			}
			let Some(j) = (i + 1..fields.len()).find(|&j| keyed(j)) else {
				continue;
			};
			let Some(switch) = switch_of(j).filter(|_| matches!(plans[j], Plan::Keep)) else {
				continue;
			};
			let switch = if refs == 1 {
				switch.clone()
			} else {
				match plan::fuse(switch, &scope, tag) {
					Some(fused) => fused,
					None => continue,
				}
			};
			let Some(tag_ty) = tag_field.get("type") else {
				continue;
			};
			let Some(kind) = ctx.classify_tag(tag_ty) else {
				continue;
			};
			if !keys_fit(&kind, &switch) {
				continue;
			}
			if j == i + 1 && !matches!(kind, TagKind::OptionScalar) {
				plans[i] = Plan::Absorbed;
				plans[j] = Plan::Inline(kind, switch, tag_ty.clone(), fields[j].0.clone());
			} else {
				plans[i] = Plan::Virtual(fields[j].0.clone());
				plans[j] = Plan::External(switch);
			}
		}

		let emit = |ctx: &mut UpgradeCtx, i: usize| -> Result<Vec<ast::Value>> {
			let (name, field) = &fields[i];
			match &plans[i] {
				Plan::Inline(TagKind::Mapper(mapper), switch, _, _) => {
					info!("container field {name} looks like legacy protodef mapper+switch, it will be replaced with union");
					crate::upgrade::merge_mapper_switch(mapper, switch, ctx)
				}
				Plan::Inline(_, switch, tag, _) => {
					crate::upgrade::merge_scalar_switch(tag, switch, ctx)
				}
				Plan::External(switch) => crate::upgrade::upgrade_switch(switch, ctx),
				Plan::Keep | Plan::Virtual(_) | Plan::Absorbed => {
					let ty = field.get("type").ok_or_else(|| anyhow!("missing type"))?;
					ctx.upgrade_type(ty.clone())
				}
			}
		};

		let name_of = |i: usize| match &plans[i] {
			Plan::Inline(_, _, _, name) => name.clone(),
			_ => fields[i].0.clone(),
		};
		// effective = fields surviving the mapper merge
		let effective: Vec<usize> = (0..fields.len())
			.filter(|&i| !matches!(plans[i], Plan::Absorbed))
			.collect();
		let switch_valued: BTreeSet<String> = effective
			.iter()
			.filter(|&&i| match &plans[i] {
				Plan::Inline(..) | Plan::External(_) => true,
				Plan::Keep => fields[i].1.get("type").and_then(plan::head) == Some("switch"),
				_ => false,
			})
			.map(|&i| name_of(i))
			.collect();
		// A container left with a single field is a transparent wrapper: emit that
		// field's type directly (e.g. a packet *is* its body union, not a struct
		// wrapping it). Unframed so the inner type's references account for the missing
		// container level.
		if let [only] = effective.as_slice() {
			let name = name_of(*only);
			return emit(ctx, *only).with_context(|| format!("transparent container <{name}>"));
		}

		let mut statements = Vec::new();
		for i in effective {
			let name = name_of(i);
			let name = name.as_str();
			let ty = ctx
				.in_container(names.clone(), switch_valued.clone(), |ctx| emit(ctx, i))
				.with_context(|| format!("container field <{name}> value"))?;
			let args = ast::ItemArgs::builder().arg(ast::Value::new_simple_item(name));
			let item = match &plans[i] {
				Plan::Virtual(union) => {
					ast::Item::simple_builder("virtual_field").argsb(args.named(
						"value",
						ast::Value::FieldRef(ast::FieldRef {
							up: 0,
							path: ast::BaseFieldRef(vec![
								ast::FieldRefSegment::Down {
									to: Ident::new(union),
									ignore_delegation: false,
								},
								ast::FieldRefSegment::Virtual(Ident::new("tag")),
							]),
						}),
					))
				}
				_ => ast::Item::simple_builder("field").argsb(args),
			};
			statements.push(Statement::simple(item.then(ty)));
		}
		Ok(ast::Item::simple_builder("container")
			.block(ast::Block {
				attributes: Default::default(),
				statements,
				root: false,
			})
			.build_values())
	}
}
struct Switch;
impl Upgrade for Switch {
	fn name(&self) -> String {
		"switch".to_owned()
	}
	// 	fn sig(&self) -> NativeSig {
	// // NativeSig { args: , generics: (), value_generic: (), specializer: (), accepts_body: () }
	// 	}
}
struct Mapper;
impl Upgrade for Mapper {
	fn name(&self) -> String {
		"mapper".to_owned()
	}
}
