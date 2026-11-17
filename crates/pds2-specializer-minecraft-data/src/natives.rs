use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use pds2_core::ast::{self, Ident};
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr};
use pds2_core::specializer::eval::{int_of, Args, Target, Val};
use pds2_specializer_bytes::{Encoder, Encoding, Input};
use pds2_core::specializer::expr::{describe, Elaborator, ValueModel};
use pds2_core::specializer::reflect::{list_of, use_of, variant_of};
use pds2_core::specializer::{
	construct_vec, pds_io, vec_of_value, visit_specialization, AstHir, BoxCost, Embed,
	LowerCx, Names, NativeCodegen, NativeCodegenCtx, NativeLayout, SlotVisitor, ViewPolicy,
	into_view_vec, view_vec,
	VALUE,
};
use pds2_specializer_protodef::upgrade::{NativeSig, NativeSigVariant, Upgrade, UpgradeCtx};

pub struct EntityMetadataLoop;

pub struct TopBitSetTerminatedArray;

impl NativeLayout for TopBitSetTerminatedArray {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		visit_specialization(native, VALUE, Embed::Indirect, v);
	}
}

impl NativeCodegen<Rust> for TopBitSetTerminatedArray {
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		_id: TypeId,
		_native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		construct_vec(cx, c)
	}
	fn inline_ty(
		&self,
		_id: TypeId,
		native: &mir::NativeType,
		names: &dyn Names<Rust>,
	) -> Option<rust::Tokens> {
		vec_of_value(native, names)
	}
	fn view_policy(&self) -> ViewPolicy {
		ViewPolicy::Composite
	}
	fn view_take(&self) -> bool {
		true
	}
	fn view_heap(&self) -> bool {
		true
	}
	fn into_view(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<Option<rust::Tokens>> {
		let value = ctx.specialization("Value").context("topBitSetTerminatedArray value")?;
		Ok(Some(into_view_vec(ctx, value)))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let value_use = ctx
			.specialization_use("Value")
			.context("topBitSetTerminatedArray value")?;
		let value = value_use.target;
		let ty = vec_of_value(ctx.native, ctx.names).context("topBitSetTerminatedArray type")?;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let perror = &rust::import("crate::pds", "Error");
		let lt = &ctx.lt();
		let this = &ctx.this();
		Ok(quote! {
			$((ctx.args_struct)())
			pub type $this = $ty;
			pub fn serialize$lt(_0: &$this, output: &mut $poutput) -> $presult<()> {
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
			pub fn deserialize$lt(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<$this> {
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
			pub fn size_of$lt(_0: &$this) -> usize {
				let mut n = 0;
				for e in _0.iter() { n += $(ctx.names.size_of(value))(e); }
				n
			}
			$(if ctx.view => $(view_vec(ctx, value)))
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

pub struct RegistryEntryHolder;

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
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		construct_holder_rust(cx, id, native, c)
	}
	fn view_policy(&self) -> ViewPolicy {
		ViewPolicy::Composite
	}
	fn into_view(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<Option<rust::Tokens>> {
		let Some((iname, ity_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolder needs an inline field from its body");
		};
		let (owned, path) = (&ctx.owned_this(), &ctx.into_path());
		let inline = ctx.slot_new(iname, quote!($(ctx.names.into_view_fn(ity_use.target))(v)));
		Ok(Some(ctx.into_view_fn(quote! {
			match _0 {
				$owned::Id(id) => $path::Id(*id),
				$owned::Inline(v) => $path::Inline($inline),
			}
		})))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolder needs the `varint` native"))?;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let lt = &ctx.lt();
		let this = &ctx.this();
		let owned = &ctx.owned_this();
		// the projected use-site body field is the inline (id 0) representation
		let Some((iname, ity_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolder needs an inline field from its body");
		};
		let ity = ity_use.target;
		let read_inline = ctx.slot_new(iname, quote!($(ctx.deserialize_call(ity_use))?));
		let view_inline = ctx.slot_new(iname, quote!($(ctx.names.view_fn(ity))(v)));
		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub enum $(ctx.nominal()) {
				Id($(ctx.names.ty(varint))),
				Inline($(ctx.slot_ty(iname, ity))),
			}
			pub fn serialize$lt(_0: &$this, output: &mut $poutput) -> $presult<()> {
				match _0 {
					Ty::Id(id) => $(ctx.names.serialize(varint))(&(id + 1), output),
					Ty::Inline(v) => {
						$(ctx.names.serialize(varint))(&0, output)?;
						$(ctx.names.serialize(ity))(v, output)
					}
				}
			}
			pub fn deserialize$lt(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<$this> {
				let n = $(ctx.names.deserialize(varint))(input)?;
				Ok(if n == 0 {
					Ty::Inline($read_inline)
				} else {
					Ty::Id(n - 1)
				})
			}
			pub fn size_of$lt(_0: &$this) -> usize {
				match _0 {
					Ty::Id(id) => $(ctx.names.size_of(varint))(&(id + 1)),
					Ty::Inline(v) => $(ctx.names.size_of(varint))(&0) + $(ctx.names.size_of(ity))(v),
				}
			}
			$(if ctx.view =>
				pub fn view<'i>(_0: &'i $owned) -> Ty<'i> {
					match _0 {
						$owned::Id(id) => Ty::Id(*id),
						$owned::Inline(v) => Ty::Inline($view_inline),
					}
				}
			)
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
				arg_types: BTreeMap::from_iter([("end".to_owned(), "End")]),
				generics: BTreeSet::from_iter(["End".to_owned()]),
				..Default::default()
			},
			..Default::default()
		}
	}

	// https://github.com/PrismarineJS/node-minecraft-protocol/blob/master/src/datatypes/compiler-minecraft.js
	fn upgrade(&self, data: &[serde_json::Value], ctx: &mut UpgradeCtx) -> Result<Vec<ast::Value>> {
		ensure!(data.len() == 1, "entityMetadataLoop should have exactly one argument");
		let obj = data[0]
			.as_object()
			.ok_or_else(|| anyhow!("entityMetadataLoop needs an object"))?;
		let end = obj
			.get("endVal")
			.ok_or_else(|| anyhow!("entityMetadataLoop missing endVal"))?;
		let ty = obj
			.get("type")
			.ok_or_else(|| anyhow!("entityMetadataLoop missing type"))?;
		let end = ast::Item::simple_builder("u8")
			.argsb(ast::ItemArgs::builder().arg(ctx.upgrade_value(end)?))
			.build_values()
			.remove(0);
		Ok(ast::Item::simple_builder("entityMetadataLoop")
			.argsb(ast::ItemArgs::builder().named("end", end))
			.then(
				ctx.wrapped(|ctx| ctx.upgrade_type(ty.clone()))
					.context("entityMetadataLoop value")?,
			))
	}
}

pub struct UUID;

impl Upgrade for UUID {
	fn name(&self) -> String {
		"UUID".to_owned()
	}
}

fn viewed(how: &'static str) -> NativeSig {
	NativeSig {
		view: Some(how),
		..Default::default()
	}
}

pub struct Opaque(pub &'static str);

impl Upgrade for Opaque {
	fn name(&self) -> String {
		self.0.to_owned()
	}
	fn sig(&self) -> NativeSig {
		match self.0 {
			"nbt" | "optionalNbt" => viewed("take"),
			_ => NativeSig::default(),
		}
	}
}

pub struct AnonOptionalNbt;

impl Upgrade for AnonOptionalNbt {
	fn name(&self) -> String {
		"anonOptionalNbt".to_owned()
	}
	fn sig(&self) -> NativeSig {
		viewed("take")
	}
}

pub struct AnonymousNbt;

impl Upgrade for AnonymousNbt {
	fn name(&self) -> String {
		"anonymousNbt".to_owned()
	}
	fn sig(&self) -> NativeSig {
		viewed("take")
	}
}

pub struct LpVec3;

impl Upgrade for LpVec3 {
	fn name(&self) -> String {
		"lpVec3".to_owned()
	}
	fn sig(&self) -> NativeSig {
		viewed("borrow")
	}
}

pub struct RegistryEntryHolderSet;

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
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		native: &mir::NativeType,
		c: &mir::Construction,
	) -> Result<rust::Tokens> {
		construct_holder_rust(cx, id, native, c)
	}
	fn view_policy(&self) -> ViewPolicy {
		ViewPolicy::Composite
	}
	fn view_heap(&self) -> bool {
		true
	}
	fn view_moves(&self) -> bool {
		true
	}
	fn into_view(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<Option<rust::Tokens>> {
		let Some((nname, nty_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolderSet needs a named field from its body");
		};
		let (owned, path) = (&ctx.owned_this(), &ctx.into_path());
		let named = ctx.slot_new(nname, quote!($(ctx.names.into_view_fn(nty_use.target))(v)));
		Ok(Some(ctx.into_view_fn(quote! {
			match _0 {
				$owned::Named(v) => $path::Named($named),
				$owned::Ids(ids) => $path::Ids(::core::mem::take(ids)),
			}
		})))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let varint = (ctx.resolve)("varint")
			.ok_or_else(|| anyhow!("registryEntryHolderSet needs the `varint` native"))?;
		let (_, poutput, presult) = pds_io();
		let (poutput, presult) = (&poutput, &presult);
		let lt = &ctx.lt();
		let this = &ctx.this();
		let owned = &ctx.owned_this();
		let Some((nname, nty_use)) = ctx.native.fields.first() else {
			bail!("registryEntryHolderSet needs a named field from its body");
		};
		let nty = nty_use.target;
		let read_named = ctx.slot_new(nname, quote!($(ctx.deserialize_call(nty_use))?));
		let view_named = ctx.slot_new(nname, quote!($(ctx.names.view_fn(nty))(v)));
		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub enum $(ctx.nominal()) {
				Named($(ctx.slot_ty(nname, nty))),
				Ids(Vec<$(ctx.names.ty(varint))>),
			}
			pub fn serialize$lt(_0: &$this, output: &mut $poutput) -> $presult<()> {
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
			pub fn deserialize$lt(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<$this> {
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
			pub fn size_of$lt(_0: &$this) -> usize {
				match _0 {
					Ty::Named(v) => 1 + $(ctx.names.size_of(nty))(v),
					Ty::Ids(ids) => {
						let mut n = $(ctx.names.size_of(varint))(&(ids.len() + 1).try_into().unwrap_or_default());
						for id in ids.iter() { n += $(ctx.names.size_of(varint))(id); }
						n
					}
				}
			}
			$(if ctx.view =>
				pub fn view<'i>(_0: &'i $owned) -> Ty<'i> {
					match _0 {
						$owned::Named(v) => Ty::Named($view_named),
						$owned::Ids(ids) => Ty::Ids(ids.clone()),
					}
				}
			)
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

impl Encoder for TopBitSetTerminatedArray {
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let items = list_of(v.c)?;
		ensure!(!items.is_empty(), "a top-bit terminated array needs at least one item");
		let value = use_of(v.native, VALUE)?;
		for (i, item) in items.iter().enumerate() {
			let mut buf = Vec::new();
			cx.encode_use(value, item, &v.scope(), &mut buf)
				.with_context(|| format!("item {i}"))?;
			let first = buf
				.first_mut()
				.ok_or_else(|| anyhow!("item {i} encodes to no bytes"))?;
			ensure!(*first & 0x80 == 0, "item {i} already sets the top bit");
			if i + 1 < items.len() {
				*first |= 0x80;
			}
			out.extend(buf);
		}
		Ok(())
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let value = use_of(t.native, VALUE)?;
		let mut items = Vec::new();
		loop {
			let mut masked = input.peek().to_vec();
			let first = masked
				.first_mut()
				.ok_or_else(|| anyhow!("byte {}: item {} is missing", input.position(), items.len()))?;
			let more = *first & 0x80 != 0;
			*first &= 0x7f;
			let mut item_input = Input::at(&masked, input.position());
			let item = cx
				.decode_use(value, &t.scope(), &mut item_input)
				.with_context(|| format!("item {}", items.len()))?;
			input.take(item_input.offset())?;
			items.push(item);
			if !more {
				return Ok(Construction::List(items));
			}
		}
	}
}

impl ValueModel for TopBitSetTerminatedArray {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		pds2_core::specializer::list_value(cx, id, native, value)
	}
}

fn holder_value(
	cx: &Elaborator,
	id: TypeId,
	native: &mir::NativeType,
	value: &Expr,
	list_key: &str,
	list_kind: &str,
	body_kind: &str,
) -> Result<Construction> {
	let Expr::Record(given) = value else {
		bail!("{} is written as `({list_key}: ...)` or `(<field>: ...)`", cx.name(id));
	};
	let [(key, e)] = given.as_slice() else {
		bail!("{} value names one representation, got {} fields", cx.name(id), given.len());
	};
	let varint = cx.resolve_name("varint")?;
	if key.as_str() == list_key {
		let payload = match list_kind {
			"ids" => {
				let Expr::List(items) = e else {
					bail!("`{list_key}` is written as a list `[...]`");
				};
				Expr::List(
					items
						.iter()
						.map(|i| cx.coerce(i.clone(), varint))
						.collect::<Result<_>>()?,
				)
			}
			_ => cx.coerce(e.clone(), varint)?,
		};
		return Ok(Construction::Variant(Ident::new(list_kind), vec![payload]));
	}
	let (name, ty) = native
		.fields
		.first()
		.ok_or_else(|| anyhow!("{} has no body field", cx.name(id)))?;
	ensure!(
		key == name,
		"{} is written as `({list_key}: ...)` or `({name:?}: ...)`, got {key:?}",
		cx.name(id)
	);
	let payload = cx
		.coerce(e.clone(), ty.target)
		.with_context(|| format!("field {name:?}"))?;
	Ok(Construction::Variant(Ident::new(body_kind), vec![payload]))
}

impl ValueModel for RegistryEntryHolder {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		holder_value(cx, id, native, value, "id", "id", "inline")
	}
	fn literal(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		native: &mir::NativeType,
		c: &Construction,
	) -> Result<Expr> {
		holder_literal(cx, native, c, "id")
	}
}

fn holder_literal(
	cx: &Elaborator,
	native: &mir::NativeType,
	c: &Construction,
	list_kind: &str,
) -> Result<Expr> {
	let (kind, payload) = variant_of(c)?;
	let [value] = payload else {
		bail!("holder variant {kind:?} with {} values", payload.len());
	};
	let key = if kind.as_str() == list_kind {
		kind.clone()
	} else {
		holder_slot(native)?.clone()
	};
	Ok(Expr::Record(vec![(key, cx.literal(value)?)]))
}

impl Encoder for RegistryEntryHolder {
	fn min_size(&self, cx: &Encoding, _id: TypeId, _native: &mir::NativeType) -> usize {
		cx.min_size_of("varint")
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let varint = cx.resolve_name("varint")?;
		match variant_of(v.c)? {
			(k, [id]) if k.as_str() == "id" => {
				let n = int_of(id).ok_or_else(|| anyhow!("id {} is not a number", describe(id)))?;
				cx.encode_number(varint, n + 1, out)
			}
			(_, [value]) => {
				cx.encode_number(varint, 0, out)?;
				let (_, ty) = v.native.fields.first().ok_or_else(|| anyhow!("no body field"))?;
				cx.encode_use(ty, value, &v.scope(), out)
			}
			(k, p) => bail!("registryEntryHolder variant {k:?} with {} values", p.len()),
		}
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let varint = cx.resolve_name("varint")?;
		let n = cx.decode(varint, &Args::new(), input)?;
		match int_of(&n) {
			Some(0) => {
				let (_, ty) = t.native.fields.first().ok_or_else(|| anyhow!("no body field"))?;
				let value = cx.decode_use(ty, &t.scope(), input)?;
				Ok(Construction::Variant(Ident::new("inline"), vec![value]))
			}
			Some(n) => Ok(Construction::Variant(
				Ident::new("id"),
				vec![cx.number(varint, n - 1)?],
			)),
			None => bail!("{} is not a number", describe(&n)),
		}
	}
}

impl ValueModel for RegistryEntryHolderSet {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		holder_value(cx, id, native, value, "ids", "ids", "named")
	}
	fn literal(
		&self,
		cx: &Elaborator,
		_id: TypeId,
		native: &mir::NativeType,
		c: &Construction,
	) -> Result<Expr> {
		holder_literal(cx, native, c, "ids")
	}
}

impl Encoder for RegistryEntryHolderSet {
	fn min_size(&self, cx: &Encoding, _id: TypeId, _native: &mir::NativeType) -> usize {
		cx.min_size_of("varint")
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let varint = cx.resolve_name("varint")?;
		match variant_of(v.c)? {
			(k, [Expr::List(ids)]) if k.as_str() == "ids" => {
				cx.encode_number(varint, ids.len() as i128 + 1, out)?;
				for id in ids {
					cx.encode(id, &Args::new(), out)?;
				}
				Ok(())
			}
			(_, [value]) => {
				cx.encode_number(varint, 0, out)?;
				let (_, ty) = v.native.fields.first().ok_or_else(|| anyhow!("no body field"))?;
				cx.encode_use(ty, value, &v.scope(), out)
			}
			(k, p) => bail!("registryEntryHolderSet variant {k:?} with {} values", p.len()),
		}
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let varint = cx.resolve_name("varint")?;
		let n = cx.decode(varint, &Args::new(), input)?;
		match cx.count(&n)? {
			0 => {
				let (_, ty) = t.native.fields.first().ok_or_else(|| anyhow!("no body field"))?;
				let value = cx.decode_use(ty, &t.scope(), input)?;
				Ok(Construction::Variant(Ident::new("named"), vec![value]))
			}
			n => {
				let ids = (0..n - 1)
					.map(|i| {
						cx.decode(varint, &Args::new(), input)
							.with_context(|| format!("id {i}"))
					})
					.collect::<Result<_>>()?;
				Ok(Construction::Variant(Ident::new("ids"), vec![Expr::List(ids)]))
			}
		}
	}
}

fn holder_slot(native: &mir::NativeType) -> Result<&Ident> {
	native
		.fields
		.first()
		.map(|(n, _)| n)
		.ok_or_else(|| anyhow!("holder without a body field"))
}

pub fn construct_holder_rust(
	cx: &LowerCx<Rust>,
	id: TypeId,
	native: &mir::NativeType,
	c: &Construction,
) -> Result<rust::Tokens> {
	let (kind, payload) = variant_of(c)?;
	let ty = cx.names.ty(id);
	Ok(match (kind.as_str(), payload) {
		("id", [n]) => quote!($ty::Id($((cx.lower)(n)?))),
		("ids", [list]) => quote!($ty::Ids($((cx.lower)(list)?))),
		("inline", [v]) => {
			quote!($ty::Inline($(cx.names.slot_new(id, holder_slot(native)?, (cx.lower)(v)?))))
		}
		("named", [v]) => {
			quote!($ty::Named($(cx.names.slot_new(id, holder_slot(native)?, (cx.lower)(v)?))))
		}
		(k, p) => bail!("holder variant {k:?} with {} values", p.len()),
	})
}

pub fn holder_payload_js(
	cx: &LowerCx<JavaScript>,
	c: &Construction,
	typed_ids: Option<&'static str>,
) -> Result<(String, js::Tokens)> {
	let (kind, payload) = variant_of(c)?;
	let [value] = payload else {
		bail!("holder variant {kind:?} with {} values", payload.len());
	};
	let value = match (kind.as_str(), value, typed_ids) {
		("ids", Expr::List(items), Some(array)) => {
			let items = items
				.iter()
				.map(|i| (cx.lower)(i))
				.collect::<Result<Vec<_>>>()?;
			quote!(new $array([$(for i in items join(, ) => $i)]))
		}
		_ => (cx.lower)(value)?,
	};
	Ok((kind.as_str().to_owned(), value))
}
