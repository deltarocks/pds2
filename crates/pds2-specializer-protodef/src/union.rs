use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use pds2_core::specializer::decode::Input;
use pds2_core::specializer::encode::{Encoder, Encoding, Scope, Target, Val};
use pds2_core::specializer::expr::{value_eq, Elaborator, ValueModel};
use pds2_core::specializer::{
	AstHir, BoxCost, Embed, LowerCx, Members, NativeCodegen, NativeCodegenCtx, NativeLayout,
	Projection, SlotVisitor, ViewPolicy,
};
use crate::natives::{is_constant, sized_count};
use crate::upgrade::{NativeSig, NativeSigVariant, Upgrade};
use pds2_core::ast::{FieldRefSegment, Ident};
use pds2_core::generator::common::sanitize;
use pds2_core::hir::{self, HirSpecializerCtx, TypeId};
use pds2_core::mir::{self, Construction, Expr, TypeKind, TypeMap, Value};

pub struct Union;

pub enum TagView<'a> {
	External(&'a mir::Field),
	Inline(&'a mir::Type),
}

pub struct UnionView<'a> {
	pub tag: TagView<'a>,
	pub tag_type: TypeId,
	pub arms: Vec<(Value, &'a Ident, &'a mir::Type)>,
	pub default: Option<(&'a Ident, &'a mir::Type)>,
}

fn match_value(v: &Value) -> Value {
	let Value::String(s) = v else {
		return v.clone();
	};
	let parsed = match s.strip_prefix("0x") {
		Some(hex) => i128::from_str_radix(hex, 16),
		None => s.parse::<i128>(),
	};
	match parsed {
		Ok(n) => Value::Signed(n),
		Err(_) => Value::UnionDescriminator(Ident::new(s)),
	}
}

pub fn view(native: &mir::NativeType) -> Option<UnionView<'_>> {
	let (spec, variant) = native.specializer.as_ref()?;
	if spec.as_str() != "union" {
		return None;
	}
	let tag = match variant.as_str() {
		"external" => TagView::External(
			native
				.ref_args
				.iter()
				.find(|(n, _)| n.as_str() == "tag")
				.and_then(|(_, e)| e.as_field())?,
		),
		"inline" => TagView::Inline(
			native
				.specialization
				.iter()
				.find(|(n, _)| n.as_str() == "Tag")
				.map(|(_, t)| t)?,
		),
		_ => return None,
	};
	let tag_type = match &tag {
		TagView::External(f) => f.0.last().map(|i| i.to.get()).unwrap_or_default(),
		TagView::Inline(t) => t.target,
	};
	let mut arms = Vec::new();
	let mut default = None;
	for (item, (name, ty)) in native.body.iter().zip(native.fields.iter()) {
		match item.kind.as_str() {
			"variant" => arms.push((match_value(item.constant("match")?), name, ty)),
			"default" => default = Some((name, ty)),
			_ => return None,
		}
	}
	Some(UnionView {
		tag,
		tag_type,
		arms,
		default,
	})
}

pub fn view_of(types: &TypeMap, id: TypeId) -> Option<UnionView<'_>> {
	match &types[id].kind {
		TypeKind::Native(n) => view(n),
		_ => None,
	}
}

impl AstHir for Union {
	fn project(&self, ctx: &HirSpecializerCtx) -> Result<Projection> {
		let variants: BTreeSet<&str> = ctx
			.body
			.iter()
			.filter(|f| f.kind.as_str() == "variant")
			.filter_map(|f| f.name.as_ref())
			.map(Ident::as_str)
			.collect();
		let mut out = Projection::default();
		for f in ctx.body {
			let ty =
				f.ty.clone()
					.ok_or_else(|| anyhow!("union arm {:?} has no type", f.name))?;
			let name = match f.kind.as_str() {
				"variant" => f.expect_name()?.clone(),
				"default" => {
					let mut name = "default".to_owned();
					while variants.contains(name.as_str()) {
						name.push('_');
					}
					Ident::new(&name)
				}
				k => bail!("unexpected union item {k:?}"),
			};
			out.fields.push((name, ty));
		}
		match ctx.variant.as_str() {
			"inline" => {
				let tag = ctx
					.types
					.get(&Ident::new("Tag"))
					.ok_or_else(|| anyhow!("inline union needs a `Tag` type"))?;
				out.virtual_fields.push((Ident::new("tag"), tag.clone()));
			}
			"external" => {
				ensure!(
					ctx.args
						.iter()
						.any(|(n, e)| n.as_str() == "tag" && matches!(e, hir::Expr::Field(_))),
					"union `tag` should be a field reference"
				);
				out.arg_virtuals.push(Ident::new("tag"));
			}
			v => bail!("unknown union variant {v:?}"),
		}
		Ok(out)
	}
}

impl NativeLayout for Union {
	fn visit(&self, native: &mir::NativeType, v: &mut dyn SlotVisitor) {
		let Some(u) = view(native) else {
			return;
		};
		for (_, name, ty) in &u.arms {
			v.slot(name, ty.target, Embed::Boxable(BoxCost::Low));
		}
		if let Some((name, ty)) = u.default {
			v.slot(&Ident::new("@tag"), u.tag_type, Embed::Fixed);
			v.slot(name, ty.target, Embed::Boxable(BoxCost::Low));
		}
	}
	fn members(&self, native: &mir::NativeType) -> Members {
		let Some(u) = view(native) else {
			return Members::default();
		};
		Members {
			variants: u
				.arms
				.iter()
				.map(|(_, name, _)| (*name).clone())
				.chain(u.default.map(|(name, _)| name.clone()))
				.collect(),
			..Default::default()
		}
	}
}

impl Upgrade for Union {
	fn name(&self) -> String {
		"union".to_owned()
	}
	fn sig(&self) -> NativeSig {
		NativeSig {
			specializer: Some("union".to_owned()),
			default: NativeSigVariant {
				accepts_body: Some(
					"item(variant) { argument(match); type(Value) => void; }; item(default) { anonymous; type(Value) => void; };",
				),
				..Default::default()
			},
			variants: BTreeMap::from_iter([
				(
					"external".to_owned(),
					NativeSigVariant {
						args: BTreeSet::from_iter(["tag".to_owned()]),
						..Default::default()
					},
				),
				(
					"inline".to_owned(),
					NativeSigVariant {
						generics: BTreeSet::from_iter(["Tag".to_owned()]),
						..Default::default()
					},
				),
			]),
			..Default::default()
		}
	}
}

const TAG: &str = "__tag";

impl ValueModel for Union {
	fn construct(
		&self,
		cx: &Elaborator,
		id: TypeId,
		native: &mir::NativeType,
		value: &Expr,
	) -> Result<Construction> {
		let u = view(native).ok_or_else(|| anyhow!("malformed union"))?;
		let void = |ty: &mir::Type| cx.types.is_void(ty.target);
		let arm = |name: &Ident| {
			u.arms
				.iter()
				.find(|(_, n, _)| sanitize(n.as_str()) == sanitize(name.as_str()))
				.map(|(_, n, ty)| (*n, *ty))
				.ok_or_else(|| {
					let known: Vec<&str> = u.arms.iter().map(|(_, n, _)| n.as_str()).collect();
					anyhow!("{} has no variant {name:?}, expected one of {known:?}", cx.name(id))
				})
		};
		match value {
			Expr::Const(Value::UnionDescriminator(name)) => {
				let (arm, ty) = arm(name)?;
				ensure!(void(ty), "variant {name:?} carries a value, write it as `({name:?}: ...)`");
				Ok(Construction::Variant(arm.clone(), vec![]))
			}
			Expr::Const(value) => {
				if let Some((_, arm, ty)) = u.arms.iter().find(|(v, _, _)| value_eq(v, value)) {
					ensure!(void(ty), "variant {arm:?} carries a value, write it as `({arm:?}: ...)`");
					return Ok(Construction::Variant((*arm).clone(), vec![]));
				}
				let (default, ty) = u
					.default
					.ok_or_else(|| anyhow!("{} has no variant matching {value:?}", cx.name(id)))?;
				ensure!(void(ty), "the default of {} carries a value", cx.name(id));
				let tag = cx.build(u.tag_type, &Expr::Const(value.clone()))?;
				Ok(Construction::Variant(default.clone(), vec![tag]))
			}
			Expr::Record(fields) => {
				let [(name, payload)] = fields.as_slice() else {
					bail!(
						"a {} value names one variant, `(variant: value)`, got {} fields",
						cx.name(id),
						fields.len()
					);
				};
				if let Some((default, ty)) = u.default.filter(|(d, _)| d == &name) {
					ensure!(
						void(ty),
						"a default carrying a value can't be written yet, {name:?} of {}",
						cx.name(id)
					);
					let tag = cx
						.coerce(payload.clone(), u.tag_type)
						.with_context(|| format!("{name:?} tag"))?;
					return Ok(Construction::Variant(default.clone(), vec![tag]));
				}
				let (arm, ty) = arm(name)?;
				ensure!(!void(ty), "variant {name:?} carries no value, write it as `{name:?}`");
				let payload = cx
					.coerce(payload.clone(), ty.target)
					.with_context(|| format!("variant {name:?}"))?;
				Ok(Construction::Variant(arm.clone(), vec![payload]))
			}
			v => bail!("{} can't be written as {}", cx.name(id), pds2_core::specializer::expr::describe(v)),
		}
	}
	fn casts(&self, _cx: &Elaborator, _id: TypeId, native: &mir::NativeType) -> Vec<TypeId> {
		view(native).map(|u| vec![u.tag_type]).unwrap_or_default()
	}
	fn exprs(&self, _cx: &Elaborator, _id: TypeId, native: &mir::NativeType) -> Vec<Expr> {
		let Some(u) = view(native) else {
			return vec![];
		};
		u.arms
			.iter()
			.map(|(value, _, _)| {
				Expr::Eq(
					Box::new(Expr::Var(Ident::new(TAG), u.tag_type)),
					Box::new(Expr::Const(value.clone())),
				)
			})
			.collect()
	}
}

fn tag_of(cx: &Encoding, v: &Val) -> Result<Expr> {
	let u = view(v.native).ok_or_else(|| anyhow!("malformed union"))?;
	let (name, payload) = variant_of(v.c)?;
	if u.default.is_some_and(|(d, _)| d == name) {
		return payload
			.first()
			.cloned()
			.ok_or_else(|| anyhow!("default of {} without its tag", cx.name(v.id)));
	}
	let (value, _, _) = u
		.arms
		.iter()
		.find(|(_, n, _)| *n == name)
		.ok_or_else(|| anyhow!("{} has no variant {name:?}", cx.name(v.id)))?;
	cx.elab
		.build(u.tag_type, &Expr::Const(value.clone()))
		.with_context(|| format!("tag of variant {name:?}"))
}

impl Encoder for Union {
	fn min_size(&self, cx: &Encoding, _id: TypeId, native: &mir::NativeType) -> usize {
		let Some(u) = view(native) else {
			return 0;
		};
		let tag = match u.tag {
			TagView::Inline(t) => cx.min_size(t.target),
			TagView::External(_) => 0,
		};
		let payload = u
			.arms
			.iter()
			.map(|(_, _, t)| *t)
			.chain(u.default.map(|(_, t)| t))
			.map(|t| cx.min_size(t.target))
			.min()
			.unwrap_or(0);
		tag.saturating_add(payload)
	}
	fn encode(&self, cx: &Encoding, v: &Val, out: &mut Vec<u8>) -> Result<()> {
		let u = view(v.native).ok_or_else(|| anyhow!("malformed union"))?;
		let (name, payload) = variant_of(v.c)?;
		let scope = v.scope();
		let default = u.default.filter(|(d, _)| *d == name);
		let tag = match u.tag {
			TagView::Inline(t) => {
				let tag = tag_of(cx, v)?;
				cx.encode_use(t, &tag, &scope, out).context("tag")?;
				tag
			}
			TagView::External(f) => cx.eval(&Expr::Field(f.clone()), &scope).context("tag")?,
		};
		let conds = cx.elab.exprs(v.id)?;
		let vars = [(Ident::new(TAG), tag.clone())];
		let tagged = Scope {
			vars: &vars,
			..scope
		};
		let mut matching = None;
		for ((_, arm, _), cond) in u.arms.iter().zip(&conds) {
			if cx.truthy(cond, &tagged)? {
				matching = Some(*arm);
				break;
			}
		}
		ensure!(
			matching == Some(name) || (matching.is_none() && default.is_some()),
			"variant {name:?} of {} disagrees with its tag {}, which selects {}",
			cx.name(v.id),
			pds2_core::specializer::expr::describe(&tag),
			matching.map_or("the default".to_owned(), |m| format!("{m:?}"))
		);
		let ty = match default {
			Some((_, ty)) => ty,
			None => u
				.arms
				.iter()
				.find(|(_, n, _)| *n == name)
				.map(|(_, _, ty)| *ty)
				.ok_or_else(|| anyhow!("{} has no variant {name:?}", cx.name(v.id)))?,
		};
		let value = match (default, payload) {
			(Some(_), [_, value]) | (None, [value]) => value.clone(),
			_ => cx.elab.build(ty.target, &Expr::Record(vec![]))?,
		};
		cx.encode_use(ty, &value, &scope, out)
			.with_context(|| format!("variant {name:?}"))
	}
	fn decode(&self, cx: &Encoding, t: &Target, input: &mut Input) -> Result<Construction> {
		let u = view(t.native).ok_or_else(|| anyhow!("malformed union"))?;
		let scope = t.scope();
		let tag = match u.tag {
			TagView::Inline(ty) => cx.decode_use(ty, &scope, input).context("tag")?,
			TagView::External(f) => cx.eval(&Expr::Field(f.clone()), &scope).context("tag")?,
		};
		let conds = cx.elab.exprs(t.id)?;
		let vars = [(Ident::new(TAG), tag.clone())];
		let tagged = Scope {
			vars: &vars,
			..scope
		};
		let mut matching = None;
		for ((_, arm, ty), cond) in u.arms.iter().zip(&conds) {
			if cx.truthy(cond, &tagged)? {
				matching = Some((*arm, *ty));
				break;
			}
		}
		let (name, ty, mut payload) = match (matching, u.default) {
			(Some((arm, ty)), _) => (arm, ty, vec![]),
			(None, Some((default, ty))) => (default, ty, vec![tag]),
			(None, None) => bail!(
				"{} has no variant for tag {}",
				cx.name(t.id),
				pds2_core::specializer::expr::describe(&tag)
			),
		};
		if !cx.types().is_void(ty.target) {
			let partial = Construction::Variant(name.clone(), payload.clone());
			let scope = Scope {
				value: Some((t.id, &partial)),
				..scope
			};
			let value = cx
				.decode_use(ty, &scope, input)
				.with_context(|| format!("variant {name:?}"))?;
			payload.push(value);
		}
		Ok(Construction::Variant(name.clone(), payload))
	}
	fn virtual_field(&self, cx: &Encoding, v: &Val, name: &Ident) -> Result<Expr> {
		ensure!(name.as_str() == "tag", "{} has no virtual field {name:?}", cx.name(v.id));
		tag_of(cx, v)
	}
	fn cast(&self, cx: &Encoding, v: &Val, to: TypeId) -> Result<Expr> {
		let tag = tag_of(cx, v)?;
		ensure!(
			tag.ty().map(|t| cx.elab.resolve(t)) == Some(cx.elab.resolve(to)),
			"{} can't be cast to {}",
			cx.name(v.id),
			cx.name(to)
		);
		Ok(tag)
	}
}

fn variant_of<'c>(c: &'c Construction) -> Result<(&'c Ident, &'c [Expr])> {
	match c {
		Construction::Variant(name, payload) => Ok((name, payload)),
		c => bail!("union is constructed from a variant, got {c:?}"),
	}
}

impl NativeCodegen<Rust> for Union {
	fn view_policy(&self) -> ViewPolicy {
		ViewPolicy::Composite
	}
	fn into_view(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<Option<rust::Tokens>> {
		let u = view(ctx.native).ok_or_else(|| anyhow!("malformed union"))?;
		let names = ctx.names;
		let owned = &ctx.owned_this();
		let path = &ctx.into_path();
		let mut arms = Vec::new();
		for (_, variant, vty) in &u.arms {
			let v = ctx.variant(variant.as_str());
			if ctx.types.is_void(vty.target) {
				arms.push(quote!($owned::$(&v) => $path::$(&v),));
			} else {
				let payload = ctx.slot_new(variant, quote!($(names.into_view_fn(vty.target))(__v)));
				arms.push(quote!($owned::$(&v)(__v) => $path::$(&v)($payload),));
			}
		}
		if let Some((dname, dty)) = u.default {
			let d = ctx.variant(dname.as_str());
			let tag = quote!($(names.into_view_fn(u.tag_type))(__t));
			if ctx.types.is_void(dty.target) {
				arms.push(quote!($owned::$(&d)(__t) => $path::$(&d)($tag),));
			} else {
				let payload = ctx.slot_new(dname, quote!($(names.into_view_fn(dty.target))(__v)));
				arms.push(quote!($owned::$(&d)(__t, __v) => $path::$(&d)($tag, $payload),));
			}
		}
		let head = if arms.is_empty() {
			quote!(match *_0)
		} else {
			quote!(match _0)
		};
		Ok(Some(ctx.into_view_fn(quote! {
			$head {
				$(for a in arms join($['\r']) => $a)
			}
		})))
	}
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let u = view(ctx.native).ok_or_else(|| anyhow!("malformed union"))?;
		let input = ctx.types;
		let names = ctx.names;
		let poutput = &rust::import("crate::pds", "Output");
		let presult = &rust::import("crate::pds", "Result");
		let lt = &ctx.lt();
		let this = &ctx.this();
		let owned = &ctx.owned_this();
		let mut view_arms: Vec<rust::Tokens> = Vec::new();
		let args_of = |t: &mir::Type| (ctx.pass_args)(t);
		let is_inline = matches!(u.tag, TagView::Inline(_));
		let read_tag = match u.tag {
			TagView::External(f) => {
				let tag_param = match f.0.first().map(|i| &i.segment) {
					Some(FieldRefSegment::Arg(name)) => name.clone(),
					other => bail!("union tag should be an argument by codegen: {other:?}"),
				};
				quote!(let __tag = args.$(ctx.arg(tag_param.as_str()));)
			}
			TagView::Inline(t) => quote! {
				let __tagv = $(names.deserialize(t.target))(
					input,
					$(args_of(t))
				)?;
				let __tag = &__tagv;
			},
		};
		let write_tag = match u.tag {
			TagView::External(_) => quote!(),
			TagView::Inline(t) => quote! {
				$(names.serialize(t.target))(&virtual_tag(_0), output)?;
			},
		};
		let tag_own_size = match u.tag {
			TagView::External(_) => quote!(0),
			TagView::Inline(t) => quote!($(names.size_of(t.target))(&virtual_tag(_0))),
		};
		let tag_ty = u.tag_type;
		let conds = ctx
			.exprs
			.iter()
			.map(|e| (ctx.lower)(e, false))
			.collect::<Result<Vec<_>>>()?;
		ensure!(conds.len() == u.arms.len(), "one condition per union arm");
		let tag_value = |value: &Value| -> Result<rust::Tokens> {
			match ctx.elab.construct(tag_ty, value) {
				Some(e) => (ctx.lower)(&e, false),
				None => Ok(quote!(todo!("reconstructing a non-scalar tag"))),
			}
		};
		let payload_ty = |t: TypeId, boxed: bool| -> rust::Tokens {
			if boxed {
				quote!(Box<$(names.ty(t))>)
			} else {
				quote!($(names.ty(t)))
			}
		};
		let min = Encoding::new(ctx.elab).min_size(ctx.id);
		let expect = (min > 0).then(|| quote!(input.expect($(min.to_string()))?;));
		let de_value = |u: &mir::Type, boxed: bool| -> rust::Tokens {
			let read = quote!($(names.deserialize(u.target))(input, $(args_of(u)))?);
			if boxed {
				quote!(Box::new($read))
			} else {
				read
			}
		};
		let mut variants_decl: Vec<rust::Tokens> = Vec::new();
		let mut ser_arms: Vec<rust::Tokens> = Vec::new();
		let mut size_arms: Vec<rust::Tokens> = Vec::new();
		let mut vtag_arms: Vec<rust::Tokens> = Vec::new();
		let mut assert_arms: Vec<rust::Tokens> = Vec::new();
		let mut de_branches: Vec<rust::Tokens> = Vec::new();
		let mut name_arms: Vec<rust::Tokens> = Vec::new();
		for ((value, variant, vty), cond) in u.arms.iter().zip(&conds) {
			let v = ctx.variant(variant.as_str());
			if input.is_void(vty.target) {
				name_arms.push(quote!(Ty::$(&v) => $(quoted(variant.as_str())),));
				view_arms.push(quote!($owned::$(&v) => Ty::$(&v),));
			} else {
				name_arms.push(quote!(Ty::$(&v)(_) => $(quoted(variant.as_str())),));
				let payload = ctx.slot_new(variant, quote!($(names.view_fn(vty.target))(__v)));
				view_arms.push(quote!($owned::$(&v)(__v) => Ty::$(&v)($payload),));
			}
			if input.is_void(vty.target) {
				variants_decl.push(quote!($(&v),));
				ser_arms.push(quote!(Ty::$(&v) => Ok(()),));
				size_arms.push(quote!(Ty::$(&v) => 0,));
				vtag_arms.push(quote!(Ty::$(&v) => $(tag_value(value)?),));
				assert_arms.push(quote!(Ty::$(&v) => $cond,));
				de_branches.push(quote!(if $cond { return Ok(Ty::$(&v)); }));
			} else {
				let boxed = ctx.boxed(variant);
				variants_decl.push(quote!($(&v)($(payload_ty(vty.target, boxed))),));
				let check = match sized_count(ctx.types, vty).filter(|e| is_constant(e)) {
					Some(count) => quote! {
						$(names.item(vty.target, "check_count"))(__v, $((ctx.read)(count, false)))?;
					},
					None => quote!(),
				};
				ser_arms.push(quote! {
					Ty::$(&v)(__v) => {
						$check
						$(names.serialize(vty.target))(__v, output)
					}
				});
				size_arms.push(quote!(Ty::$(&v)(__v) => $(names.size_of(vty.target))(__v),));
				vtag_arms.push(quote!(Ty::$(&v)(_) => $(tag_value(value)?),));
				assert_arms.push(quote!(Ty::$(&v)(_) => $cond,));
				de_branches.push(quote! {
					if $cond {
						return Ok(Ty::$(&v)($(de_value(vty, boxed))));
					}
				});
			}
		}
		let de_default: rust::Tokens = if let Some((dname, dty)) = u.default {
			let d = ctx.variant(dname.as_str());
			name_arms.push(quote!(Ty::$(&d)(..) => $(quoted(dname.as_str())),));
			let view_tag = quote!($(names.view_fn(tag_ty))(__t));
			if input.is_void(dty.target) {
				view_arms.push(quote!($owned::$(&d)(__t) => Ty::$(&d)($view_tag),));
			} else {
				let payload = ctx.slot_new(dname, quote!($(names.view_fn(dty.target))(__v)));
				view_arms.push(quote!($owned::$(&d)(__t, __v) => Ty::$(&d)($view_tag, $payload),));
			}
			let boxed = ctx.boxed(dname);
			let dpay = de_value(dty, boxed);
			let tag_owned = if is_inline {
				quote!(__tagv)
			} else {
				quote!(__tag.clone())
			};
			if input.is_void(dty.target) {
				variants_decl.push(quote!($(&d)($(names.ty(tag_ty))),));
				ser_arms.push(quote!(Ty::$(&d)(_) => Ok(()),));
				size_arms.push(quote!(Ty::$(&d)(_) => 0,));
				vtag_arms.push(quote!(Ty::$(&d)(__t) => __t.clone(),));
				assert_arms.push(quote!(Ty::$(&d)(_) => true,));
				quote!(Ok(Ty::$(&d)($tag_owned)))
			} else {
				variants_decl.push(quote!($(&d)($(names.ty(tag_ty)), $(payload_ty(dty.target, boxed))),));
				ser_arms.push(
					quote!(Ty::$(&d)(_, __v) => $(names.serialize(dty.target))(__v, output),),
				);
				size_arms.push(quote!(Ty::$(&d)(_, __v) => $(names.size_of(dty.target))(__v),));
				vtag_arms.push(quote!(Ty::$(&d)(__t, _) => __t.clone(),));
				assert_arms.push(quote!(Ty::$(&d)(..) => true,));
				quote!(Ok(Ty::$(&d)($tag_owned, $dpay)))
			}
		} else {
			quote!(Err($(rust::import("crate::pds", "Error"))::InvalidValue))
		};
		let is_empty = u.arms.is_empty() && u.default.is_none();
		let match_head = &if is_empty {
			quote!(match *_0)
		} else {
			quote!(match _0)
		};
		let expose_tag = is_inline || (ctx.virtual_used)("tag") || !ctx.cast_to.is_empty();
		Ok(quote! {
			$((ctx.args_struct)())
			$(ctx.derive())
			pub enum $(ctx.nominal()) {
				$(for v in &variants_decl join($['\r']) => $v)
			}
			pub fn serialize$lt(_0: &$this, output: &mut $poutput) -> $presult<()> {
				$(if is_empty => match *_0 {})
				$(if !is_empty {
					$write_tag
					$match_head {
						$(for a in &ser_arms join($['\r']) => $a)
					}
				})
			}
			pub fn deserialize$lt(input: &mut $(ctx.input()), $((ctx.args_param)())) -> $presult<$this> {
				$expect
				$read_tag
				$(for b in &de_branches join($['\r']) => $b)
				$de_default
			}
			pub fn size_of$lt(_0: &$this) -> usize {
				$(if is_empty => match *_0 {})
				$(if !is_empty {
					$tag_own_size + $match_head {
						$(for a in &size_arms join($['\r']) => $a)
					}
				})
			}
			pub fn name$lt(_0: &$this) -> &'static str {
				$match_head {
					$(for a in &name_arms join($['\r']) => $a)
				}
			}
			$(if !is_inline && !is_empty =>
				pub fn debug_assert_tag$lt(_0: &$this, __tag: &$(names.ty(tag_ty))) {
					debug_assert!(
						$match_head {
							$(for a in &assert_arms join($['\r']) => $a)
						},
						"union variant does not match its external tag",
					);
				}
			)
			$(if expose_tag =>
				pub fn virtual_tag$lt(_0: &$this) -> $(names.ty(tag_ty)) {
					$(if is_empty => match *_0 {})
					$(if !is_empty {
						$match_head {
							$(for a in &vtag_arms join($['\r']) => $a)
						}
					})
				}
			)
			$(if ctx.view =>
				pub fn view<'i>(_0: &'i $owned) -> Ty<'i> {
					$match_head {
						$(for a in &view_arms join($['\r']) => $a)
					}
				}
			)
		})
	}
	fn construct(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		native: &mir::NativeType,
		c: &Construction,
	) -> Result<rust::Tokens> {
		let (name, payload) = variant_of(c)?;
		let is_default = view(native)
			.and_then(|u| u.default)
			.is_some_and(|(d, _)| d == name);
		let payload = payload
			.iter()
			.map(|e| (cx.lower)(e))
			.collect::<Result<Vec<_>>>()?;
		let payload = match (is_default, payload.as_slice()) {
			(false, [value]) => vec![cx.names.slot_new(id, name, value.clone())],
			_ => payload,
		};
		Ok(quote! {
			$(cx.names.ty(id))::$(cx.names.variant(id, name.as_str()))$(if !payload.is_empty() => ($(for p in payload join(, ) => $p)))
		})
	}
	fn cast(
		&self,
		cx: &LowerCx<Rust>,
		id: TypeId,
		_native: &mir::NativeType,
		value: rust::Tokens,
		_to: TypeId,
	) -> Result<rust::Tokens> {
		Ok(quote!($(cx.names.item(id, "virtual_tag"))(&$value)))
	}
}

impl NativeCodegen<JavaScript> for Union {
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let u = view(ctx.native).ok_or_else(|| anyhow!("malformed union"))?;
		let input = ctx.types;
		let names = ctx.names;
		let pinput = &ctx.runtime("Input");
		let poutput = &ctx.runtime("Output");
		let perror = &ctx.runtime("PdsError");
		let has_args = !ctx.params.is_empty();
		let args_of = |t: &mir::Type| -> js::Tokens {
			let a = (ctx.pass_args)(t);
			if a.is_empty() {
				quote!()
			} else {
				quote!(, $a)
			}
		};
		let is_inline = matches!(u.tag, TagView::Inline(_));
		let is_empty = u.arms.is_empty() && u.default.is_none();
		let tag_ty = u.tag_type;
		let conds = ctx
			.exprs
			.iter()
			.map(|e| (ctx.lower)(e, false))
			.collect::<Result<Vec<_>>>()?;
		ensure!(conds.len() == u.arms.len(), "one condition per union arm");
		let tag_value = |value: &Value| -> Result<js::Tokens> {
			match ctx.elab.construct(tag_ty, value) {
				Some(e) => (ctx.lower)(&e, false),
				None => Ok(
					quote!((() => { throw new $perror("InvalidValue", "reconstructing a non-scalar tag"); })()),
				),
			}
		};
		let read_tag = match u.tag {
			TagView::External(f) => {
				let tag_param = match f.0.first().map(|i| &i.segment) {
					Some(FieldRefSegment::Arg(name)) => name.clone(),
					other => bail!("union tag should be an argument by codegen: {other:?}"),
				};
				quote!(const __tag = args.$(tag_param.as_str());)
			}
			TagView::Inline(t) => quote! {
				const __tag = $(names.deserialize(t.target))(input$(args_of(t)));
			},
		};
		let write_tag = match u.tag {
			TagView::External(_) => quote!(),
			TagView::Inline(t) => quote! {
				$(names.serialize(t.target))(virtual_tag(_0), output);
			},
		};
		let tag_own_size = match u.tag {
			TagView::External(_) => quote!(0),
			TagView::Inline(t) => quote!($(names.size_of(t.target))(virtual_tag(_0))),
		};

		let mut variants: Vec<js::Tokens> = Vec::new();
		let mut ser: Vec<js::Tokens> = Vec::new();
		let mut size: Vec<js::Tokens> = Vec::new();
		let mut vtag: Vec<js::Tokens> = Vec::new();
		let mut asserts: Vec<js::Tokens> = Vec::new();
		let mut de: Vec<js::Tokens> = Vec::new();
		for ((value, variant, vty), cond) in u.arms.iter().zip(&conds) {
			let kind: js::Tokens = quote!($(quoted(variant.as_str())));
			if input.is_void(vty.target) {
				variants.push(quote!({ kind: $(&kind) }));
				ser.push(quote!(case $(&kind): return;));
				size.push(quote!(case $(&kind): return __n;));
				de.push(quote! {
					if ($cond) {
						return { kind: $(&kind) };
					}
				});
			} else {
				variants.push(quote!({ kind: $(&kind); value: $(names.ty(vty.target)) }));
				let check = match sized_count(ctx.types, vty).filter(|e| is_constant(e)) {
					Some(count) => quote! {
						$(names.item(vty.target, "check_count"))(_0.value, $((ctx.read)(count, false)));
					},
					None => quote!(),
				};
				ser.push(
					quote!(case $(&kind): $check $(names.serialize(vty.target))(_0.value, output); return;),
				);
				size.push(
					quote!(case $(&kind): return __n + $(names.size_of(vty.target))(_0.value);),
				);
				de.push(quote! {
					if ($cond) {
						return { kind: $(&kind), value: $(names.deserialize(vty.target))(input$(args_of(vty))) };
					}
				});
			}
			vtag.push(quote!(case $(&kind): return $(tag_value(value)?);));
			asserts.push(quote!(case $(&kind): ok = $cond; break;));
		}
		let de_default = if let Some((dname, dty)) = u.default {
			let kind: js::Tokens = quote!($(quoted(dname.as_str())));
			if input.is_void(dty.target) {
				variants.push(quote!({ kind: $(&kind); tag: $(names.ty(tag_ty)) }));
				ser.push(quote!(case $(&kind): return;));
				size.push(quote!(case $(&kind): return __n;));
				vtag.push(quote!(case $(&kind): return _0.tag;));
				quote!(return { kind: $(&kind), tag: __tag };)
			} else {
				variants.push(quote!({ kind: $(&kind); tag: $(names.ty(tag_ty)); value: $(names.ty(dty.target)) }));
				ser.push(
					quote!(case $(&kind): $(names.serialize(dty.target))(_0.value, output); return;),
				);
				size.push(
					quote!(case $(&kind): return __n + $(names.size_of(dty.target))(_0.value);),
				);
				vtag.push(quote!(case $(&kind): return _0.tag;));
				quote!(return { kind: $(&kind), tag: __tag, value: $(names.deserialize(dty.target))(input$(args_of(dty))) };)
			}
		} else {
			quote!(throw new $perror("InvalidValue", "no union arm matches the tag");)
		};

		let expose_tag = is_inline || (ctx.virtual_used)("tag") || !ctx.cast_to.is_empty();
		let args_param = || -> js::Tokens {
			if has_args {
				quote!(, $((ctx.args_param)()))
			} else {
				quote!()
			}
		};
		Ok(if is_empty {
			quote! {
				$((ctx.args_struct)())
				export type Ty = never;
				export function serialize(_0: Ty, _output: $poutput): void {
					throw new $perror("InvalidValue", "empty union");
				}
				export function deserialize(input: $pinput$(args_param())): Ty {
					$read_tag
					throw new $perror("InvalidValue", "empty union");
				}
				export function size_of(_0: Ty): number {
					throw new $perror("InvalidValue", "empty union");
				}
				$(if expose_tag =>
					export function virtual_tag(_0: Ty): $(names.ty(tag_ty)) {
						throw new $perror("InvalidValue", "empty union");
					}
				)
			}
		} else {
			quote! {
				$((ctx.args_struct)())
				export type Ty =
					$(for v in &variants join($['\r']) => | $v)
				;
				export function serialize(_0: Ty, output: $poutput): void {
					$write_tag
					switch (_0.kind) {
						$(for a in &ser join($['\r']) => $a)
					}
				}
				export function deserialize(input: $pinput$(args_param())): Ty {
					$read_tag
					$(for b in &de join($['\r']) => $b)
					$de_default
				}
				export function size_of(_0: Ty): number {
					const __n = $tag_own_size;
					switch (_0.kind) {
						$(for a in &size join($['\r']) => $a)
					}
				}
				$(if !is_inline =>
					export function assert_tag(_0: Ty, __tag: $(names.ty(tag_ty))): void {
						let ok = true;
						switch (_0.kind) {
							$(for a in &asserts join($['\r']) => $a)
						}
						if (!ok) {
							throw new $perror("InvalidValue", "union variant does not match its external tag");
						}
					}
				)
				$(if expose_tag =>
					export function virtual_tag(_0: Ty): $(names.ty(tag_ty)) {
						switch (_0.kind) {
							$(for a in &vtag join($['\r']) => $a)
						}
					}
				)
			}
		})
	}
	fn construct(
		&self,
		cx: &LowerCx<JavaScript>,
		_id: TypeId,
		native: &mir::NativeType,
		c: &Construction,
	) -> Result<js::Tokens> {
		let (name, payload) = variant_of(c)?;
		let is_default = view(native)
			.and_then(|u| u.default)
			.is_some_and(|(d, _)| d == name);
		Ok(match (is_default, payload) {
			(_, []) => quote!({ kind: $(quoted(name.as_str())) }),
			(true, [tag]) => quote!({ kind: $(quoted(name.as_str())), tag: $((cx.lower)(tag)?) }),
			(false, [value]) => quote!({ kind: $(quoted(name.as_str())), value: $((cx.lower)(value)?) }),
			_ => bail!("union variant {name:?} constructed with {} values", payload.len()),
		})
	}
	fn cast(
		&self,
		cx: &LowerCx<JavaScript>,
		id: TypeId,
		_native: &mir::NativeType,
		value: js::Tokens,
		_to: TypeId,
	) -> Result<js::Tokens> {
		Ok(quote!($(cx.names.item(id, "virtual_tag"))($value)))
	}
}
