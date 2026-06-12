//! Specializers: Rust implementations that turn a use-site body of a native type
//! into a concrete list of typed fields.
//!
//! A `def_native` declares `body;` to accept a body and `@specializer name` to pick
//! the implementation. The `apply_specializers` pass calls [`Specializer::fields`]
//! during HIR desugaring (after path resolution) to project the body into fields.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::rust;
use genco::prelude::*;
use tracing::info;

use crate::ast::{self, Ident, Statement};
use crate::upgrade::{NativeSig, NativeSigVariant, UpgradeCtx};

use crate::hir::{self, HirSpecializerCtx, TypeId};
use crate::mir;

/// Object-safe: registered per native, invoked during AST->HIR to produce type fields.
pub trait AstHir {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeneratorKind {
	Rust,
	Typescript,
}

/// Name resolution for generated Rust: genco imports for another pds type's items,
/// so cross-module `use` statements are collected and deduplicated by genco.
pub trait RustNames {
	fn ty(&self, id: TypeId) -> rust::Import;
	fn serialize(&self, id: TypeId) -> rust::Import;
	fn deserialize(&self, id: TypeId) -> rust::Import;
	fn size_of(&self, id: TypeId) -> rust::Import;
}

/// Everything a native codegen needs to emit the module contents for one concrete
/// (monomorphized) native instance.
pub struct NativeCodegenCtx<'a> {
	/// the selected def_native overload
	pub variant: &'a Ident,
	pub native: &'a mir::NativeType,
	/// value params of the def (`argument(size)` + closure-converted captures), with
	/// resolved types
	pub params: &'a [(Ident, TypeId)],
	pub rust: &'a dyn RustNames,
	/// resolve a top-level type by name (e.g. `u8` for raw byte IO)
	pub resolve: &'a dyn Fn(&str) -> Option<TypeId>,
	/// the `pub struct Args<'a> { ... }` declaration for this def's params (empty
	/// tokens when there are none)
	pub args_struct: &'a dyn Fn() -> rust::Tokens,
	/// extra `deserialize` parameter (`args: Args<'_>,`) when the def has params
	pub args_param: &'a dyn Fn() -> rust::Tokens,
	/// the `Args { ... },` to pass when calling `deserialize` of the given type-use
	/// (empty tokens when the target takes no params); arg refs resolve against this
	/// def's own `args`
	pub pass_args: &'a dyn Fn(&mir::Type) -> rust::Tokens,
	/// types this native is compared against when used as a union discriminant; a
	/// tag-capable native (e.g. `option`) emits an `is_equal_{t}` helper per entry.
	pub eq_targets: &'a [TypeId],
}

impl NativeCodegenCtx<'_> {
	pub fn specialization_use(&self, name: &str) -> Result<&mir::Type> {
		self.native
			.specialization
			.iter()
			.find(|(n, _)| n.as_str() == name)
			.map(|(_, t)| t)
			.ok_or_else(|| anyhow!("generic {name:?} is not bound"))
	}
	pub fn specialization(&self, name: &str) -> Result<TypeId> {
		Ok(self.specialization_use(name)?.target)
	}
	/// a full `deserialize(input, args...)` call expression for a type-use,
	/// threading the use's bound arguments (assumes `input`/`args` in scope)
	pub fn deserialize_call(&self, u: &mir::Type) -> rust::Tokens {
		let de = self.rust.deserialize(u.target);
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
pub trait LanguageTokens: Sized {
	fn language() -> GeneratorKind;
	fn storage(reg: &Registry) -> &HashMap<String, Box<dyn NativeCodegen<Self>>>;
	fn storage_mut(reg: &mut Registry) -> &mut HashMap<String, Box<dyn NativeCodegen<Self>>>;
}
impl LanguageTokens for rust::Tokens {
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

/// Object-safe per token type: registered per specializer, produces the module
/// contents for a concrete native instance in the generator's language.
pub trait NativeCodegen<T> {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<T>;
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
	pub(crate) codegen_rust: HashMap<String, Box<dyn NativeCodegen<rust::Tokens>>>,
}

impl Registry {
	pub fn register_asthir(&mut self, name: &str, specializer: impl AstHir + 'static) {
		self.asthir.insert(name.to_owned(), Box::new(specializer));
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

	reg.register_codegen::<rust::Tokens>("bitfield", Bitfield);
	reg.register_codegen::<rust::Tokens>("bitflags", Bitflags);
	reg.register_codegen::<rust::Tokens>("array", Array);
	reg.register_codegen::<rust::Tokens>("option", OptionP);
	reg.register_codegen::<rust::Tokens>("buffer", Buffer);
	reg.register_codegen::<rust::Tokens>("pstring", PString);
	reg.register_codegen::<rust::Tokens>("entityMetadataLoop", EntityMetadataLoop);
	reg.register_codegen::<rust::Tokens>("topBitSetTerminatedArray", TopBitSetTerminatedArray);
	reg.register_codegen::<rust::Tokens>("registryEntryHolder", RegistryEntryHolder);
	reg.register_codegen::<rust::Tokens>("registryEntryHolderSet", RegistryEntryHolderSet);

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

trivial_upgrade!(U8P => "u8");
trivial_upgrade!(U16P => "u16");
trivial_upgrade!(U32P => "u32");
trivial_upgrade!(U64P => "u64");
trivial_upgrade!(Varlong => "varlong");

struct TopBitSetTerminatedArray;
impl NativeCodegen<rust::Tokens> for TopBitSetTerminatedArray {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let value = ctx
			.specialization("Value")
			.context("topBitSetTerminatedArray value")?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = Vec<$(ctx.rust.ty(value))>;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				let _ = (_0, output);
				todo!("topBitSetTerminatedArray serialization needs byte patching")
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let _ = input;
				todo!("topBitSetTerminatedArray deserialization needs input peeking")
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = 0;
				for e in _0.iter() { n += $(ctx.rust.size_of(value))(e); }
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
fn bitfield_entries(body: &[hir::BodyField]) -> Result<Vec<BitfieldEntry>> {
	let mut out = Vec::new();
	for f in body {
		let size = match f.args.get(&Ident::new("size")) {
			Some(hir::BodyArg::Int(n)) if *n > 0 => *n,
			_ => bail!(
				"bitfield field {:?} needs a positive integer `size`",
				f.name
			),
		};
		let signed = matches!(
			f.args.get(&Ident::new("signed")),
			Some(hir::BodyArg::Bool(true))
		);
		let bits = match size {
			0..=8 => 8,
			9..=16 => 16,
			17..=32 => 32,
			33..=64 => 64,
			_ => bail!("bitfield field {:?} larger than 64 bits", f.name),
		};
		out.push(BitfieldEntry {
			name: f.name.clone(),
			size: size as u32,
			signed,
			bits,
		});
	}
	Ok(out)
}

impl AstHir for Bitfield {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		bitfield_entries(ctx.body)?
			.into_iter()
			.map(|e| Ok((e.name.clone(), ctx.native(&e.prim())?)))
			.collect()
	}
}

impl NativeCodegen<rust::Tokens> for Bitfield {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
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
			pub struct Ty {
				$(for (e, (_, ty)) in entries.iter().zip(ctx.native.fields.iter()) join($['\r']) =>
					pub $(&e.name): $(ctx.rust.ty(ty.target)),
				)
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				let mut acc: u128 = 0;
				$(for e in &entries join($['\r']) =>
					acc = (acc << $(e.size.to_string())) | ((_0.$(&e.name) as u128) & $(format!("{:#x}", (1u128 << e.size) - 1)));
				)
				for i in (0..$(bytes.to_string())u32).rev() {
					$(ctx.rust.serialize(u8id))(&(((acc >> (i * 8)) & 0xff) as u8), output)?;
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let mut acc: u128 = 0;
				for _ in 0..$(bytes.to_string())u32 {
					acc = (acc << 8) | ($(ctx.rust.deserialize(u8id))(input)? as u128);
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
fn param_ty(ctx: &NativeCodegenCtx, id: TypeId) -> rust::Tokens {
	if id.is_valid() {
		quote!($(ctx.rust.ty(id)))
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
				accepts_body: true,
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
			.map(|f| Ok((f.name.clone(), bool_ty.clone())))
			.collect()
	}
}

impl NativeCodegen<rust::Tokens> for Bitflags {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let value = ctx.specialization("Value")?;
		// flag bit i = 1 << i, in declaration order
		let bit = |i: usize| format!("{:#x}", 1u64 << i);
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);

		Ok(quote! {
			$((ctx.args_struct)())
			pub struct Ty {
				$(for (name, ty) in ctx.native.fields.iter() join($['\r']) =>
					pub $name: $(ctx.rust.ty(ty.target)),
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
				$(ctx.rust.serialize(value))(&pack(_0).try_into().expect("bitflags value"), output)
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let acc: u64 = $(ctx.rust.deserialize(value))(input)?.try_into().expect("bitflags value");
				Ok(Ty {
					$(for (i, (name, _)) in ctx.native.fields.iter().enumerate() join($['\r']) =>
						$name: acc & $(bit(i)) != 0,
					)
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(ctx.rust.size_of(value))(&pack(_0).try_into().expect("bitflags value"))
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
				accepts_body: true,
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
		let ty =
			f.ty.clone()
				.ok_or_else(|| anyhow!("body field {:?} needs a type", f.name))?;
		out.push((f.name.clone(), ty));
	}
	Ok(out)
}

struct RegistryEntryHolder;
impl AstHir for RegistryEntryHolder {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		body_fields(ctx)
	}
}
impl NativeCodegen<rust::Tokens> for RegistryEntryHolder {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolder needs the `varint` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		// the projected use-site body field is the inline (id 0) representation
		let Some((iname, ity_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolder needs an inline field from its body");
		};
		let _ = iname;
		let ity = ity_use.target;
		Ok(quote! {
			$((ctx.args_struct)())
			pub enum Ty {
				Id($(ctx.rust.ty(varint))),
				Inline($(ctx.rust.ty(ity))),
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				match _0 {
					Ty::Id(id) => $(ctx.rust.serialize(varint))(&(id + 1), output),
					Ty::Inline(v) => {
						$(ctx.rust.serialize(varint))(&0, output)?;
						$(ctx.rust.serialize(ity))(v, output)
					}
				}
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let n = $(ctx.rust.deserialize(varint))(input)?;
				Ok(if n == 0 {
					Ty::Inline($(ctx.deserialize_call(ity_use))?)
				} else {
					Ty::Id(n - 1)
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				match _0 {
					Ty::Id(id) => $(ctx.rust.size_of(varint))(&(id + 1)),
					Ty::Inline(v) => $(ctx.rust.size_of(varint))(&0) + $(ctx.rust.size_of(ity))(v),
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
				accepts_body: true,
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
impl NativeCodegen<rust::Tokens> for RegistryEntryHolderSet {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolderSet needs the `varint` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let Some((nname, nty_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolderSet needs a named field from its body");
		};
		let _ = nname;
		let nty = nty_use.target;
		Ok(quote! {
			$((ctx.args_struct)())
			pub enum Ty {
				Named($(ctx.rust.ty(nty))),
				Ids(Vec<$(ctx.rust.ty(varint))>),
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				match _0 {
					Ty::Named(v) => {
						$(ctx.rust.serialize(varint))(&0, output)?;
						$(ctx.rust.serialize(nty))(v, output)
					}
					Ty::Ids(ids) => {
						$(ctx.rust.serialize(varint))(&(ids.len() + 1).try_into().expect("id set length"), output)?;
						for id in ids.iter() {
							$(ctx.rust.serialize(varint))(id, output)?;
						}
						Ok(())
					}
				}
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let n: usize = $(ctx.rust.deserialize(varint))(input)?.try_into().expect("id set length");
				Ok(if n == 0 {
					Ty::Named($(ctx.deserialize_call(nty_use))?)
				} else {
					let mut ids = Vec::with_capacity(n - 1);
					for _ in 0..n - 1 {
						ids.push($(ctx.rust.deserialize(varint))(input)?);
					}
					Ty::Ids(ids)
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				match _0 {
					Ty::Named(v) => 1 + $(ctx.rust.size_of(nty))(v),
					Ty::Ids(ids) => {
						let mut n = $(ctx.rust.size_of(varint))(&(ids.len() + 1).try_into().expect("id set length"));
						for id in ids.iter() { n += $(ctx.rust.size_of(varint))(id); }
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
				accepts_body: true,
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
impl NativeCodegen<rust::Tokens> for Array {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("array element type")?;
		let value = value_use.target;
		let de_value = |ctx: &NativeCodegenCtx| ctx.deserialize_call(value_use);
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);

		let main = match ctx.variant.as_str() {
			// length prefix read inline, as the `Count` type
			"inline" => {
				let count = ctx.specialization("Count").context("array count type")?;
				quote! {
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						$(ctx.rust.serialize(count))(&_0.len().try_into().expect("array length"), output)?;
						for e in _0.iter() {
							$(ctx.rust.serialize(value))(e, output)?;
						}
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
						let count: usize = $(ctx.rust.deserialize(count))(input)?.try_into().expect("array length");
						let mut out = Vec::with_capacity(count);
						for _ in 0..count {
							out.push($(de_value(ctx))?);
						}
						Ok(out)
					}
					pub fn size_of(_0: &Ty) -> usize {
						let mut n = $(ctx.rust.size_of(count))(&_0.len().try_into().expect("array length"));
						for e in _0.iter() { n += $(ctx.rust.size_of(value))(e); }
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
							$(ctx.rust.serialize(value))(e, output)?;
						}
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
						let count: usize = (*args.count).try_into().expect("array length");
						let mut out = Vec::with_capacity(count);
						for _ in 0..count {
							out.push($(de_value(ctx))?);
						}
						Ok(out)
					}
					pub fn size_of(_0: &Ty) -> usize {
						let mut n = 0;
						for e in _0.iter() { n += $(ctx.rust.size_of(value))(e); }
						n
					}
				}
			}
			v => bail!("unknown array overload: {v:?}"),
		};

		// the runtime length, referencable as `@size` (e.g. by a virtual length field)
		let virtual_size = match ctx.virtual_field("size") {
			Some(virt) => quote! {
				pub fn virtual_size(_0: &Ty) -> $(ctx.rust.ty(virt)) {
					_0.len().try_into().expect("array length")
				}
			},
			None => rust::Tokens::new(),
		};

		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = Vec<$(ctx.rust.ty(value))>;
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
impl NativeCodegen<rust::Tokens> for Buffer {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let u8id = (ctx.resolve)("u8").ok_or_else(|| anyhow!("buffer needs the `u8` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		let read_bytes = quote! {
			let mut out = Vec::with_capacity(count);
			for _ in 0..count {
				out.push($(ctx.rust.deserialize(u8id))(input)?);
			}
			Ok(out)
		};
		let write_bytes = quote! {
			for b in _0.iter() {
				$(ctx.rust.serialize(u8id))(b, output)?;
			}
		};
		let main = match ctx.variant.as_str() {
			"inline" => {
				let count = ctx.specialization("Count").context("buffer count type")?;
				quote! {
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						$(ctx.rust.serialize(count))(&_0.len().try_into().expect("buffer length"), output)?;
						$write_bytes
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
						let count: usize = $(ctx.rust.deserialize(count))(input)?.try_into().expect("buffer length");
						$read_bytes
					}
					pub fn size_of(_0: &Ty) -> usize {
						$(ctx.rust.size_of(count))(&_0.len().try_into().expect("buffer length")) + _0.len()
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
						let count: usize = (*args.count).try_into().expect("buffer length");
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
			pub type Ty = Vec<u8>;
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
impl NativeCodegen<rust::Tokens> for EntityMetadataLoop {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let value = ctx
			.specialization("Value")
			.context("entityMetadataLoop value")?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = Vec<$(ctx.rust.ty(value))>;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				let _ = (_0, output);
				todo!("entityMetadataLoop serialization needs the terminator key")
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let _ = input;
				todo!("entityMetadataLoop deserialization needs input peeking")
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = 1;
				for e in _0.iter() { n += $(ctx.rust.size_of(value))(e); }
				n
			}
		})
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
				args: BTreeSet::from_iter(["end".to_owned()]),
				..Default::default()
			},
			..Default::default()
		}
	}
}
struct OptionP;
impl NativeCodegen<rust::Tokens> for OptionP {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("option value type")?;
		let value = value_use.target;
		let boolean =
			(ctx.resolve)("bool").ok_or_else(|| anyhow!("option needs the `bool` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = Option<Box<$(ctx.rust.ty(value))>>;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(ctx.rust.serialize(boolean))(&_0.is_some(), output)?;
				if let Some(v) = _0 {
					$(ctx.rust.serialize(value))(v, output)?;
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				Ok(if $(ctx.rust.deserialize(boolean))(input)? {
					Some(Box::new($(ctx.deserialize_call(value_use))?))
				} else {
					None
				})
			}
			pub fn size_of(_0: &Ty) -> usize {
				let mut n = $(ctx.rust.size_of(boolean))(&_0.is_some());
				if let Some(v) = _0 { n += $(ctx.rust.size_of(value))(v); }
				n
			}
			// Union arm matching when an `option` is used as a discriminant: each
			// target is the inner value type; the present value is compared to it.
			$(for t in ctx.eq_targets join($['\r']) =>
				pub fn $(format!("is_equal_{}", t.raw()))(a: &Ty, b: &$(ctx.rust.ty(*t))) -> bool {
					a.as_deref() == Some(b)
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
struct I8P;
impl Upgrade for I8P {
	fn name(&self) -> String {
		"i8".to_owned()
	}
}
struct I16P;
impl Upgrade for I16P {
	fn name(&self) -> String {
		"i16".to_owned()
	}
}
struct I32P;
impl Upgrade for I32P {
	fn name(&self) -> String {
		"i32".to_owned()
	}
}
struct I64P;
impl Upgrade for I64P {
	fn name(&self) -> String {
		"i64".to_owned()
	}
}
struct F32P;
impl Upgrade for F32P {
	fn name(&self) -> String {
		"f32".to_owned()
	}
}
struct F64P;
impl Upgrade for F64P {
	fn name(&self) -> String {
		"f64".to_owned()
	}
}
struct Varint;
impl Upgrade for Varint {
	fn name(&self) -> String {
		"varint".to_owned()
	}
}
struct LpVec3;
impl Upgrade for LpVec3 {
	fn name(&self) -> String {
		"lpVec3".to_owned()
	}
}
struct PString;
impl NativeCodegen<rust::Tokens> for PString {
	fn generate(&self, ctx: &NativeCodegenCtx) -> Result<rust::Tokens> {
		let count = ctx.specialization("Count").context("pstring count type")?;
		let u8id = (ctx.resolve)("u8").ok_or_else(|| anyhow!("pstring needs the `u8` native"))?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);
		Ok(quote! {
			$((ctx.args_struct)())
			pub type Ty = String;
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(ctx.rust.serialize(count))(&_0.len().try_into().expect("string length"), output)?;
				for b in _0.as_bytes() {
					$(ctx.rust.serialize(u8id))(b, output)?;
				}
				Ok(())
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				let count: usize = $(ctx.rust.deserialize(count))(input)?.try_into().expect("string length");
				let mut out = Vec::with_capacity(count);
				for _ in 0..count {
					out.push($(ctx.rust.deserialize(u8id))(input)?);
				}
				Ok(String::from_utf8(out).expect("utf8 string"))
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(ctx.rust.size_of(count))(&_0.len().try_into().expect("string length")) + _0.len()
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
struct Container;
impl Upgrade for Container {
	fn name(&self) -> String {
		"container".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: None,
			default: NativeSigVariant {
				args: BTreeSet::new(),
				generics: BTreeSet::new(),
				value_generic: false,
				accepts_body: true,
			},
			..Default::default()
		}
	}
	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(data.len() == 1, "container should have argument");
		let data = data[0]
			.as_array()
			.ok_or_else(|| anyhow!("expected container data to be array"))?;
		fn field_name(item: &serde_json::Map<String, serde_json::Value>) -> Result<&str> {
			Ok(if item.contains_key("anon") {
				"ANON"
			} else {
				item.get("name")
					.ok_or_else(|| anyhow!("missing name"))?
					.as_str()
					.ok_or_else(|| anyhow!("expected name to be string"))?
			})
		}
		let mut names = BTreeSet::new();
		for item in data {
			if let Some(item) = item.as_object() {
				names.insert(field_name(item)?.to_owned());
			}
		}

		// protodef `mapper` (discriminant) + `switch(compareTo: mapper)` is a pds
		// inline-tag union; merge them when the mapper feeds exactly one switch and
		// nothing else refers to it.
		fn typed<'a>(
			item: &'a serde_json::Value,
			head: &str,
		) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
			let arr = item.get("type")?.as_array()?;
			if arr.first()?.as_str()? != head {
				return None;
			}
			arr.get(1)?.as_object()
		}
		fn count_refs(data: &[serde_json::Value], name: &str) -> usize {
			fn scan(v: &serde_json::Value, name: &str, acc: &mut usize) {
				match v {
					serde_json::Value::Object(m) => {
						for (k, val) in m {
							if (k == "compareTo" || k == "count" || k.ends_with("To"))
								&& val.as_str() == Some(name)
							{
								*acc += 1;
							}
							scan(val, name, acc);
						}
					}
					serde_json::Value::Array(a) => a.iter().for_each(|e| scan(e, name, acc)),
					_ => {}
				}
			}
			let mut acc = 0;
			for f in data {
				scan(f, name, &mut acc);
			}
			acc
		}
		// switch field name -> the mapper spec it absorbs
		let mut merge: std::collections::HashMap<
			&str,
			&serde_json::Map<String, serde_json::Value>,
		> = std::collections::HashMap::new();
		let mut absorbed: BTreeSet<&str> = BTreeSet::new();
		for item in data {
			let Some(sw) = typed(item, "switch") else {
				continue;
			};
			let Some(ct) = sw.get("compareTo").and_then(serde_json::Value::as_str) else {
				continue;
			};
			let mapper = data
				.iter()
				.find(|m| m.get("name").and_then(serde_json::Value::as_str) == Some(ct))
				.and_then(|m| typed(m, "mapper"));
			if let Some(mapper) = mapper {
				if count_refs(data, ct) == 1 {
					let sw_name = item
						.as_object()
						.map(field_name)
						.transpose()?
						.unwrap_or("ANON");
					merge.insert(sw_name, mapper);
					absorbed.insert(ct);
				}
			}
		}

		let upgrade_one = |ctx: &mut UpgradeCtx,
		                   item: &serde_json::Value,
		                   name: &str,
		                   framed: bool|
		 -> Result<Vec<ast::Value>> {
			let go = |ctx: &mut UpgradeCtx| -> Result<Vec<ast::Value>> {
				if let Some(mapper) = merge.get(name).copied() {
					let switch = typed(item, "switch").expect("switch located during planning");
					info!("container field {name} looks like legacy protodef mapper+switch, it will be replaced with union");
					crate::upgrade::merge_mapper_switch(mapper, switch, ctx)
				} else {
					let ty = item.get("type").ok_or_else(|| anyhow!("missing type"))?;
					ctx.upgrade_type(ty.clone())
				}
			};
			if framed {
				ctx.in_container(names.clone(), go)
			} else {
				go(ctx)
			}
		};

		// effective = fields surviving the mapper merge
		let mut effective: Vec<&serde_json::Value> = Vec::new();
		for item in data {
			let obj = item
				.as_object()
				.ok_or_else(|| anyhow!("object field should be object"))?;
			if !absorbed.contains(field_name(obj)?) {
				effective.push(item);
			}
		}
		// A container left with a single field is a transparent wrapper: emit that
		// field's type directly (e.g. a packet *is* its body union, not a struct
		// wrapping it). Unframed so the inner type's references account for the missing
		// container level.
		if let [only] = effective.as_slice() {
			let name = field_name(only.as_object().unwrap())?.to_owned();
			return upgrade_one(ctx, only, &name, false)
				.with_context(|| format!("transparent container <{name}>"));
		}

		let mut statements = Vec::new();
		for item in effective {
			let obj = item.as_object().unwrap();
			let name = field_name(obj)?;
			let ty = upgrade_one(ctx, item, name, true)
				.with_context(|| format!("container field <{name}> value"))?;
			statements.push(Statement::simple(
				ast::Item::simple_builder("field")
					.argsb(ast::ItemArgs::builder().arg(ast::Value::new_simple_item(name)))
					.then(ty),
			));
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
