//! Specializers: Rust implementations that turn a use-site body of a native type
//! into a concrete list of typed fields.
//!
//! A `def_native` declares `body;` to accept a body and `@specializer name` to pick
//! the implementation. The `apply_specializers` pass calls [`Specializer::fields`]
//! during HIR desugaring (after path resolution) to project the body into fields.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, ensure, Context as _, Result};

use crate::ast::{self, Ident, Statement};
use crate::upgrade::{NativeSig, NativeSigVariant, UpgradeCtx};

use crate::hir::{self, HirSpecializerCtx};

/// Object-safe: registered per native, invoked during AST->HIR to produce type fields.
pub trait AstHir {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>>;
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
}

impl Registry {
	pub fn register_asthir(&mut self, name: &str, specializer: impl AstHir + 'static) {
		self.asthir.insert(name.to_owned(), Box::new(specializer));
	}
	pub fn register_upgrade(&mut self, specializer: impl Upgrade + 'static) {
		let spec = Box::new(specializer);
		self.upgrade.insert(spec.name(), spec);
	}
}

pub fn register_protodef_types(reg: &mut Registry) {
	reg.register_asthir("bitfield", Bitfield);
	reg.register_asthir("bitflags", Bitflags);

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
}

/// Packed named integers, MSB-first. Each field projects to the smallest native int
/// that fits its bit width.
struct Bitfield;
impl AstHir for Bitfield {
	fn fields(&self, ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, hir::TypeKind)>> {
		let mut out = Vec::new();
		for f in ctx.body {
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
			let int = format!("{}{bits}", if signed { "i" } else { "u" });
			out.push((f.name.clone(), ctx.native(&int)?));
		}
		Ok(out)
	}
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
