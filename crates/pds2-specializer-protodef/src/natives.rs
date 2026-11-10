use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{rust, Rust};
use genco::prelude::*;

use pds2_core::ast::{self, Ident, Statement};
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr, Value};
use pds2_core::specializer::encode::{int_of, Args, Encoder, Encoding, Val};
use pds2_core::specializer::expr::{describe, Elaborator, ValueModel};
use pds2_core::specializer::reflect::{
	self, bytes_of, count_arg, encode_bytes, encode_items, list_of, record_fields, record_of,
	use_of, variant_name,
};
use pds2_core::specializer::{
	construct_vec, list_value, pds_io, specialization_use, vec_of_value, visit_fields,
	visit_specialization, AstHir, BoxCost, Embed, LowerCx, Names, NativeCodegen, NativeCodegenCtx,
	NativeLayout, SlotVisitor, VALUE,
};

use crate::upgrade::{NativeSig, NativeSigVariant, Upgrade, UpgradeCtx};

/// Packed named integers, MSB-first. Each field projects to the smallest native int
/// that fits its bit width.
pub struct Bitfield;

pub struct BitfieldEntry {
	pub name: Ident,
	pub size: u32,
	pub signed: bool,
	/// width of the smallest native int fitting `size`
	pub bits: u32,
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

pub fn bitfield_entries(body: &[mir::BodyItem]) -> Result<Vec<BitfieldEntry>> {
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
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		reflect::construct_struct(cx, id, c)
	}
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
pub struct Bitflags;

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
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		reflect::construct_struct(cx, id, c)
	}
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

pub struct Array;

impl NativeLayout for Array {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}

impl ValueModel for Array {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &mir::Expr,
	) -> Result<mir::Construction> {
		list_value(cx, id, native, value)
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
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		construct_vec(cx, c)
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
			encoding: None,
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

pub struct Buffer;

impl NativeLayout for Buffer {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}

impl NativeCodegen<Rust> for Buffer {
	fn construct(
		&self,
		_cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		reflect::construct_bytes_rust(c)
	}
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

pub struct OptionP;

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
		})
	}
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		let mir::Construction::Wrap(inner) = c else {
			return Ok(quote!(None));
		};
		let value = cx.names.slot_new(id, &Ident::new(VALUE), (cx.lower)(inner)?);
		Ok(quote!(Some($value)))
	}
}

impl ValueModel for OptionP {
	fn construct(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		native: &mir::NativeType,
		value: &mir::Expr,
	) -> Result<mir::Construction> {
		if matches!(value, mir::Expr::Const(mir::Value::UnionDescriminator(n)) if n.as_str() == "none") {
			return Ok(mir::Construction::Variant(Ident::new("none"), vec![]));
		}
		let inner = specialization_use(native, VALUE)
			.ok_or_else(|| anyhow!("option without a value type"))?
			.target;
		Ok(mir::Construction::Wrap(Box::new(cx.coerce(value.clone(), inner)?)))
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

pub struct PString;

impl NativeLayout for PString {
	fn visit(&self, _native: &mir::NativeType, _v: &mut dyn SlotVisitor) {}
}

impl NativeCodegen<Rust> for PString {
	fn construct(
		&self,
		_cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		Ok(quote!(String::from($(quoted(reflect::string_of(c)?)))))
	}
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

impl Encoder for OptionP {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let boolean = cx.resolve_name("bool")?;
		let present = matches!(v.c, Construction::Wrap(_));
		cx.encode(
			&cx.elab.build(boolean, &Expr::Const(Value::Bool(present)))?,
			&Args::new(),
			out,
		)?;
		if let Construction::Wrap(inner) = v.c {
			cx.encode_use(use_of(v.native, VALUE)?, inner, &v.scope(), out)?;
		}
		Ok(())
	}
}

impl Encoder for Array {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let items = list_of(v.c)?;
		if variant_name(v) == "inline" {
			let count = use_of(v.native, "Count")?;
			let n = cx.number(count.target, items.len() as i128)?;
			cx.encode_use(count, &n, &v.scope(), out).context("count")?;
		} else {
			count_arg(v, items.len())?;
		}
		encode_items(cx, v, items, out)
	}
	fn virtual_field(&self, cx: &Encoding, v: &Val, name: &Ident) -> Result<Expr> {
		let (_, ty) = v
			.native
			.virtual_fields
			.iter()
			.find(|(n, _)| n == name)
			.ok_or_else(|| anyhow!("{} has no virtual field {name:?}", cx.name(v.id)))?;
		ensure!(name.as_str() == "size", "unknown array virtual field {name:?}");
		cx.number(ty.target, list_of(v.c)?.len() as i128)
	}
}

impl ValueModel for Bitfield {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		record_fields(cx, id, &native.fields, value)
	}
}

impl Encoder for Bitfield {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let entries = bitfield_entries(&v.native.body)?;
		let given = record_of(v.c)?;
		let total: u32 = entries.iter().map(|e| e.size).sum();
		ensure!(total % 8 == 0, "bitfield does not pack into whole bytes: {total} bits");
		let mut acc: u128 = 0;
		for e in &entries {
			let (_, value) = given
				.iter()
				.find(|(n, _)| *n == e.name)
				.ok_or_else(|| anyhow!("bitfield value lacks {:?}", e.name))?;
			let n = int_of(value).ok_or_else(|| anyhow!("{:?} is not a number", e.name))?;
			let range = if e.signed {
				-(1i128 << (e.size - 1))..=(1i128 << (e.size - 1)) - 1
			} else {
				0..=(1i128 << e.size) - 1
			};
			ensure!(range.contains(&n), "{:?} = {n} does not fit {} bits", e.name, e.size);
			acc = (acc << e.size) | ((n as u128) & ((1u128 << e.size) - 1));
		}
		let bytes: Vec<u8> = (0..total / 8).rev().map(|i| (acc >> (i * 8)) as u8).collect();
		encode_bytes(cx, &bytes, out)
	}
}

impl ValueModel for Bitflags {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let Expr::List(items) = value else {
			bail!("{} is written as a list of set flags `[flag, ...]`", cx.name(id));
		};
		let mut set = BTreeSet::new();
		for item in items {
			let Expr::Const(Value::UnionDescriminator(flag)) = item else {
				bail!("{} is not a flag name", describe(item));
			};
			ensure!(
				native.fields.iter().any(|(n, _)| n == flag),
				"{} has no flag {flag:?}",
				cx.name(id)
			);
			ensure!(set.insert(flag.clone()), "flag {flag:?} is set twice");
		}
		let fields = native
			.fields
			.iter()
			.map(|(n, ty)| {
				let on = cx.build(ty.target, &Expr::Const(Value::Bool(set.contains(n))))?;
				Ok((n.clone(), on))
			})
			.collect::<Result<_>>()?;
		Ok(Construction::Record(fields))
	}
}

impl Encoder for Bitflags {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let mut acc: i128 = 0;
		for (i, (n, _)) in v.native.fields.iter().enumerate() {
			let (_, flag) = record_of(v.c)?
				.iter()
				.find(|(f, _)| f == n)
				.ok_or_else(|| anyhow!("bitflags value lacks {n:?}"))?;
			if int_of(flag) == Some(1) {
				acc |= 1 << i;
			}
		}
		let value = use_of(v.native, VALUE)?;
		cx.encode_use(value, &cx.number(value.target, acc)?, &v.scope(), out)
	}
}

impl ValueModel for Buffer {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		_native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		match value {
			Expr::Const(Value::Bytes(b)) => Ok(Construction::Literal(Value::Bytes(b.clone()))),
			Expr::Const(Value::String(s)) => {
				Ok(Construction::Literal(Value::Bytes(s.as_bytes().to_vec())))
			}
			v => bail!("{} is written as bytes `b\"...\"`, got {}", cx.name(id), describe(v)),
		}
	}
}

impl Encoder for Buffer {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let bytes = bytes_of(v.c)?;
		if variant_name(v) == "inline" {
			let count = use_of(v.native, "Count")?;
			let n = cx.number(count.target, bytes.len() as i128)?;
			cx.encode_use(count, &n, &v.scope(), out).context("count")?;
		} else {
			count_arg(v, bytes.len())?;
		}
		encode_bytes(cx, bytes, out)
	}
}

impl ValueModel for PString {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		_native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		match value {
			Expr::Const(Value::String(s)) => Ok(Construction::Literal(Value::String(s.clone()))),
			v => bail!("{} is written as a string, got {}", cx.name(id), describe(v)),
		}
	}
}

impl Encoder for PString {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let bytes = bytes_of(v.c)?;
		let count = use_of(v.native, "Count")?;
		let n = cx.number(count.target, bytes.len() as i128)?;
		cx.encode_use(count, &n, &v.scope(), out).context("length")?;
		encode_bytes(cx, bytes, out)
	}
}
