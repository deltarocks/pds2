//! Specializers: Rust implementations that turn a use-site body of a native type
//! into a concrete list of typed fields.
//!
//! A `def_native` declares `body;` to accept a body and `@specializer name` to pick
//! the implementation. The `apply_specializers` pass calls [`Specializer::fields`]
//! during HIR desugaring (after path resolution) to project the body into fields.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::rust;
use genco::prelude::*;

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
	/// value params of the def (`argument(size)`), with resolved types
	pub params: &'a [(Ident, TypeId)],
	pub rust: &'a dyn RustNames,
	/// resolve a top-level type by name (e.g. `u8` for raw byte IO)
	pub resolve: &'a dyn Fn(&str) -> Option<TypeId>,
}

impl NativeCodegenCtx<'_> {
	pub fn specialization(&self, name: &str) -> Result<TypeId> {
		self.native
			.specialization
			.iter()
			.find(|(n, _)| n.as_str() == name)
			.map(|(_, t)| t.target)
			.ok_or_else(|| anyhow!("generic {name:?} is not bound"))
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

	reg.register_codegen::<rust::Tokens>("bitfield", Bitfield);
	reg.register_codegen::<rust::Tokens>("bitflags", Bitflags);
	reg.register_codegen::<rust::Tokens>("array", Array);

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
trivial_upgrade!(U8P => "u8");
trivial_upgrade!(U16P => "u16");
trivial_upgrade!(U32P => "u32");
trivial_upgrade!(U64P => "u64");
trivial_upgrade!(Varlong => "varlong");
trivial_upgrade!(Void => "void");

struct TopBitSetTerminatedArray;
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
			pub fn deserialize(input: &mut $pinput) -> $presult<Ty> {
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
			pub fn deserialize(input: &mut $pinput) -> $presult<Ty> {
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
			.then(ctx.upgrade_type(ty.clone()).context("bitflags value")?))
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
struct RegistryEntryHolder;
impl Upgrade for RegistryEntryHolder {
	fn name(&self) -> String {
		"registryEntryHolder".to_owned()
	}

	fn sig(&self) -> NativeSig {
		NativeSig {
			default: NativeSigVariant {
				args: BTreeSet::from_iter(["baseName".to_owned()]),
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
impl Upgrade for RegistryEntryHolderSet {
	fn name(&self) -> String {
		"registryEntryHolderSet".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			default: NativeSigVariant {
				args: BTreeSet::from_iter(["baseName".to_owned()]),
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
		let value = ctx.specialization("Value").context("array element type")?;
		let (pinput, poutput, presult) = pds_io();
		let (pinput, poutput, presult) = (&pinput, &poutput, &presult);

		let main = match ctx.variant.as_str() {
			// length prefix read inline, as the `Size` type
			"inline" => {
				let size = ctx.specialization("Size").context("array size type")?;
				quote! {
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						$(ctx.rust.serialize(size))(&_0.len().try_into().expect("array length"), output)?;
						for e in _0.iter() {
							$(ctx.rust.serialize(value))(e, output)?;
						}
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput) -> $presult<Ty> {
						let count: usize = $(ctx.rust.deserialize(size))(input)?.try_into().expect("array length");
						let mut out = Vec::with_capacity(count);
						for _ in 0..count {
							out.push($(ctx.rust.deserialize(value))(input)?);
						}
						Ok(out)
					}
					pub fn size_of(_0: &Ty) -> usize {
						let mut n = $(ctx.rust.size_of(size))(&_0.len().try_into().expect("array length"));
						for e in _0.iter() { n += $(ctx.rust.size_of(value))(e); }
						n
					}
				}
			}
			// length supplied by the use-site (field reference or constant)
			"sized" => {
				ensure!(
					!ctx.params.is_empty(),
					"array::sized needs a `size` argument"
				);
				quote! {
					pub struct Args<'a> {
						$(for (name, ty) in ctx.params.iter() join($['\r']) =>
							pub $name: &'a $(ctx.rust.ty(*ty)),
						)
					}
					pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
						for e in _0.iter() {
							$(ctx.rust.serialize(value))(e, output)?;
						}
						Ok(())
					}
					pub fn deserialize(input: &mut $pinput, args: Args<'_>) -> $presult<Ty> {
						let count: usize = (*args.size).try_into().expect("array length");
						let mut out = Vec::with_capacity(count);
						for _ in 0..count {
							out.push($(ctx.rust.deserialize(value))(input)?);
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
				ctx.types.contains_key(&Ident::new("Size")),
				"array::inline needs a `Size:` type binding"
			),
			// length supplied by the use-site (field reference or constant)
			"sized" => ensure!(
				ctx.args.iter().any(|(n, _)| n.as_str() == "size"),
				"array::sized needs a `size:` argument"
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
			default: NativeSigVariant {
				value_generic: true,
				..Default::default()
			},
			..Default::default()
		}
	}
}
struct Buffer;
impl Upgrade for Buffer {
	fn name(&self) -> String {
		"buffer".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("buffer".to_owned()),
			default: NativeSigVariant {
				value_generic: true,
				..Default::default()
			},
			..Default::default()
		}
	}
}

struct EntityMetadataLoop;
impl Upgrade for EntityMetadataLoop {
	fn name(&self) -> String {
		"entityMetadataLoop".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("entityMetadataLoop".to_owned()),
			default: NativeSigVariant {
				value_generic: true,
				..Default::default()
			},
			..Default::default()
		}
	}
}
struct OptionP;
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
impl Upgrade for PString {
	fn name(&self) -> String {
		"pstring".to_owned()
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
		let mut statements = Vec::new();
		for item in data {
			let item = item
				.as_object()
				.ok_or_else(|| anyhow!("object field should be object"))?;
			let name = if item.contains_key("anon") {
				"ANON"
			} else {
				item.get("name")
					.ok_or_else(|| anyhow!("missing name"))?
					.as_str()
					.ok_or_else(|| anyhow!("expected name to be string"))?
			};
			let ty = item.get("type").ok_or_else(|| anyhow!("missing type"))?;
			statements.push(Statement::simple(
				ast::Item::simple_builder("field")
					.argsb(ast::ItemArgs::builder().arg(ast::Value::new_simple_item(name)))
					.then(
						ctx.upgrade_type(ty.clone())
							.with_context(|| format!("container field <{name}> value"))?,
					),
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
