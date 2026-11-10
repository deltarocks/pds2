use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use genco::lang::{js, rust, JavaScript, Rust};
use genco::prelude::*;

use super::{
	AstHir, BoxCost, Embed, NativeCodegen, NativeCodegenCtx, NativeLayout, NativeSig,
	NativeSigVariant, Projection, SlotVisitor, Upgrade,
};
use crate::ast::{FieldRefSegment, Ident};
use crate::generator::common::{eq_fn_name, eq_target, option_inner, sanitize, tag_repr, EqKind, TagRepr};
use crate::generator::rust::rname;
use crate::generator::typescript::literal;
use crate::hir::{HirSpecializerCtx, TypeId};
use crate::mir::{self, TypeKind, TypeMap, Value};

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
				.map(|(_, f)| f)?,
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
			"external" => out.arg_virtuals.push(Ident::new("tag")),
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

fn unsupported(input: &TypeMap, value: &Value, tag: TypeId) -> ! {
	panic!(
		"union arm value {value:?} cannot match tag type {:?}",
		input[tag].name.readable_name()
	)
}

impl NativeCodegen<Rust> for Union {
	fn generate(&self, ctx: &NativeCodegenCtx<Rust>) -> Result<rust::Tokens> {
		let u = view(ctx.native).ok_or_else(|| anyhow!("malformed union"))?;
		let input = ctx.types;
		let names = ctx.names;
		let pinput = &rust::import("crate::pds", "Input");
		let poutput = &rust::import("crate::pds", "Output");
		let presult = &rust::import("crate::pds", "Result");
		let args_of = |t: &mir::Type| (ctx.pass_args)(t);
		let is_inline = matches!(u.tag, TagView::Inline(_));
		let read_tag = match u.tag {
			TagView::External(f) => {
				let tag_param = match f.0.first().map(|i| &i.segment) {
					Some(FieldRefSegment::Arg(name)) => name.clone(),
					other => bail!("union tag should be an argument by codegen: {other:?}"),
				};
				quote!(let __tag = args.$(rname(&tag_param));)
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
		let cond = |value: &Value| -> rust::Tokens {
			let Some((b_id, eq_kind)) = eq_target(input, tag_ty, value) else {
				unsupported(input, value, tag_ty)
			};
			let call = names.item(tag_ty, &eq_fn_name(b_id, eq_kind));
			let arg: rust::Tokens = match value {
				Value::Signed(n) => quote!(&$(n.to_string())),
				Value::Unsigned(n) => quote!(&$(n.to_string())),
				Value::Bool(b) => quote!(&$(b.to_string())),
				Value::String(s) => quote!(&$(quoted(s.as_str())).to_owned()),
				Value::UnionDescriminator(id) => {
					let vname = Ident::new(&sanitize(id.as_str()));
					quote!(&$(names.ty(tag_ty))::$(rname(&vname)))
				}
			};
			quote!($call(__tag, $arg))
		};
		let tag_value = |value: &Value| -> rust::Tokens {
			let literal = || -> rust::Tokens {
				match value {
					Value::Signed(n) => quote!($(n.to_string())),
					Value::Unsigned(n) => quote!($(n.to_string())),
					Value::Bool(b) => quote!($(b.to_string())),
					Value::String(s) => quote!($(quoted(s.as_str())).into()),
					Value::UnionDescriminator(_) => quote!(unreachable!()),
				}
			};
			match tag_repr(input, tag_ty, value) {
				Some(TagRepr::Literal) => literal(),
				Some(TagRepr::OptionSome(opt)) => {
					let inner = names.slot_new(opt, &Ident::new("Value"), literal());
					quote!(Some($inner))
				}
				Some(TagRepr::Variant(u, n)) => quote!($(names.ty(u))::$(rname(&n))),
				None => quote!(todo!("reconstructing a non-scalar tag")),
			}
		};
		let payload_ty = |t: TypeId, boxed: bool| -> rust::Tokens {
			if boxed {
				quote!(Box<$(names.ty(t))>)
			} else {
				quote!($(names.ty(t)))
			}
		};
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
		for (value, variant, vty) in &u.arms {
			let v = rname(variant);
			if input.is_void(vty.target) {
				variants_decl.push(quote!($(&v),));
				ser_arms.push(quote!(Ty::$(&v) => Ok(()),));
				size_arms.push(quote!(Ty::$(&v) => 0,));
				vtag_arms.push(quote!(Ty::$(&v) => $(tag_value(value)),));
				assert_arms.push(quote!(Ty::$(&v) => $(cond(value)),));
				de_branches.push(quote!(if $(cond(value)) { return Ok(Ty::$(&v)); }));
			} else {
				let boxed = ctx.boxed(variant);
				variants_decl.push(quote!($(&v)($(payload_ty(vty.target, boxed))),));
				ser_arms
					.push(quote!(Ty::$(&v)(__v) => $(names.serialize(vty.target))(__v, output),));
				size_arms.push(quote!(Ty::$(&v)(__v) => $(names.size_of(vty.target))(__v),));
				vtag_arms.push(quote!(Ty::$(&v)(_) => $(tag_value(value)),));
				assert_arms.push(quote!(Ty::$(&v)(_) => $(cond(value)),));
				de_branches.push(quote! {
					if $(cond(value)) {
						return Ok(Ty::$(&v)($(de_value(vty, boxed))));
					}
				});
			}
		}
		let de_default: rust::Tokens = if let Some((dname, dty)) = u.default {
			let d = rname(dname);
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
		let eq_fns: Vec<rust::Tokens> = ctx
			.eq_targets
			.iter()
			.map(|(b, kind)| match kind {
				EqKind::SelfEq => quote! {
					pub fn $(format!("is_equal_{}", b.raw()))(a: &Ty, b: &Ty) -> bool { a == b }
				},
				EqKind::VirtualTag => quote! {
					pub fn $(format!("is_equal_{}", b.raw()))(a: &Ty, b: &$(names.ty(*b))) -> bool {
						&virtual_tag(a) == b
					}
				},
				EqKind::OptionInner => quote!(),
				EqKind::Bool => quote! {
					pub fn is_equal_bool(a: &Ty, b: &bool) -> bool { a == b }
				},
			})
			.collect();
		let expose_tag = is_inline || (ctx.virtual_used)("tag");
		Ok(quote! {
			$((ctx.args_struct)())
			#[derive(Debug, Clone, PartialEq)]
			pub enum Ty {
				$(for v in &variants_decl join($['\r']) => $v)
			}
			pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
				$(if is_empty => match *_0 {})
				$(if !is_empty {
					$write_tag
					$match_head {
						$(for a in &ser_arms join($['\r']) => $a)
					}
				})
			}
			pub fn deserialize(input: &mut $pinput, $((ctx.args_param)())) -> $presult<Ty> {
				$read_tag
				$(for b in &de_branches join($['\r']) => $b)
				$de_default
			}
			pub fn size_of(_0: &Ty) -> usize {
				$(if is_empty => match *_0 {})
				$(if !is_empty {
					$tag_own_size + $match_head {
						$(for a in &size_arms join($['\r']) => $a)
					}
				})
			}
			$(if !is_inline && !is_empty =>
				pub fn debug_assert_tag(_0: &Ty, __tag: &$(names.ty(tag_ty))) {
					debug_assert!(
						$match_head {
							$(for a in &assert_arms join($['\r']) => $a)
						},
						"union variant does not match its external tag",
					);
				}
			)
			$(if expose_tag =>
				pub fn virtual_tag(_0: &Ty) -> $(names.ty(tag_ty)) {
					$(if is_empty => match *_0 {})
					$(if !is_empty {
						$match_head {
							$(for a in &vtag_arms join($['\r']) => $a)
						}
					})
				}
			)
			$(for f in &eq_fns join($['\r']) => $f)
		})
	}
}

impl NativeCodegen<JavaScript> for Union {
	fn generate(&self, ctx: &NativeCodegenCtx<JavaScript>) -> Result<js::Tokens> {
		let u = view(ctx.native).ok_or_else(|| anyhow!("malformed union"))?;
		let input = ctx.types;
		let names = ctx.names;
		let pinput = &js::import("../pds.ts", "Input");
		let poutput = &js::import("../pds.ts", "Output");
		let perror = &js::import("../pds.ts", "PdsError");
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
		let cond = |value: &Value| -> js::Tokens {
			let Some((b, kind)) = eq_target(input, tag_ty, value) else {
				unsupported(input, value, tag_ty)
			};
			let call = names.item(tag_ty, &eq_fn_name(b, kind));
			quote!($call(__tag, $(literal(input, value, b))))
		};
		let tag_value = |value: &Value| -> js::Tokens {
			match tag_repr(input, tag_ty, value) {
				Some(TagRepr::Literal) => literal(input, value, tag_ty),
				Some(TagRepr::OptionSome(opt)) => {
					let inner = option_inner(input, opt).unwrap_or_default();
					names.slot_new(opt, &Ident::new("Value"), literal(input, value, inner))
				}
				Some(TagRepr::Variant(_, n)) => quote!({ kind: $(quoted(n.as_str())) }),
				None => {
					quote!((() => { throw new $perror("InvalidValue", "reconstructing a non-scalar tag"); })())
				}
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
		for (value, variant, vty) in &u.arms {
			let kind: js::Tokens = quote!($(quoted(variant.as_str())));
			if input.is_void(vty.target) {
				variants.push(quote!({ kind: $(&kind) }));
				ser.push(quote!(case $(&kind): return;));
				size.push(quote!(case $(&kind): return __n;));
				de.push(quote! {
					if ($(cond(value))) {
						return { kind: $(&kind) };
					}
				});
			} else {
				variants.push(quote!({ kind: $(&kind); value: $(names.ty(vty.target)) }));
				ser.push(
					quote!(case $(&kind): $(names.serialize(vty.target))(_0.value, output); return;),
				);
				size.push(
					quote!(case $(&kind): return __n + $(names.size_of(vty.target))(_0.value);),
				);
				de.push(quote! {
					if ($(cond(value))) {
						return { kind: $(&kind), value: $(names.deserialize(vty.target))(input$(args_of(vty))) };
					}
				});
			}
			vtag.push(quote!(case $(&kind): return $(tag_value(value));));
			asserts.push(quote!(case $(&kind): ok = $(cond(value)); break;));
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

		let eq_fns: Vec<js::Tokens> = ctx
			.eq_targets
			.iter()
			.map(|(b, kind)| match kind {
				EqKind::SelfEq => quote! {
					export function $(format!("is_equal_{}", b.raw()))(a: Ty, b: Ty): boolean {
						return a.kind === b.kind;
					}
				},
				EqKind::VirtualTag => quote! {
					export function $(format!("is_equal_{}", b.raw()))(a: Ty, b: $(names.ty(*b))): boolean {
						return virtual_tag(a) === b;
					}
				},
				EqKind::OptionInner => quote!(),
				EqKind::Bool => quote! {
					export function is_equal_bool(a: Ty, b: boolean): boolean {
						return Number(virtual_tag(a)) === Number(b);
					}
				},
			})
			.collect();

		let expose_tag = is_inline || (ctx.virtual_used)("tag");
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
				$(for f in &eq_fns join($['\r']) => $f)
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
				$(for f in &eq_fns join($['\r']) => $f)
			}
		})
	}
}
