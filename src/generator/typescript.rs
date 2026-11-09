use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result};
use genco::lang::{js, JavaScript};
use genco::prelude::*;

use super::common::{self, eq_target, option_inner, sanitize, scalar_of, EqKind, TagRepr};
use super::GeneratorOutput;
use crate::ast::{FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Field, FieldOrConst, Type, TypeKind, TypeMap, Value};
use crate::specializer::{Names, NativeCodegenCtx, Registry, Scalar};

type Tokens = js::Tokens;

struct TsMeta<'a> {
	raw_names: BTreeMap<TypeId, String>,
	types: &'a TypeMap,
	reg: &'a Registry,
	inlining: RefCell<BTreeSet<TypeId>>,
}

impl<'a> TsMeta<'a> {
	fn new(types: &'a TypeMap, reg: &'a Registry) -> Self {
		Self {
			raw_names: types
				.0
				.iter()
				.map(|(id, ty)| (*id, ty.name.readable_name()))
				.collect(),
			types,
			reg,
			inlining: RefCell::default(),
		}
	}
	fn name_str(&self, id: TypeId) -> String {
		assert!(id.is_valid());
		format!("pds_{}", self.raw_names[&id])
	}
	fn module(&self, id: TypeId) -> js::Import {
		let name = self.name_str(id);
		js::import(format!("./{name}.ts"), name).into_wildcard()
	}
	fn item(&self, id: TypeId, item: impl Into<String>) -> Tokens {
		let module = self.module(id);
		let item = item.into();
		quote!($module.$item)
	}
	fn inline_ty(&self, id: TypeId) -> Option<Tokens> {
		let TypeKind::Native(native) = &self.types[id].kind else {
			return None;
		};
		let (specializer, _) = native.specializer.as_ref()?;
		let codegen = self.reg.codegen::<JavaScript>(specializer.as_str())?;
		if !self.inlining.borrow_mut().insert(id) {
			return None;
		}
		let ty = codegen.inline_ty(id, native, self);
		self.inlining.borrow_mut().remove(&id);
		ty
	}
	fn nullable(&self, id: TypeId) -> bool {
		match &self.types[id].kind {
			TypeKind::Alias(t) => self.nullable(t.target),
			_ => self.types.is_void(id) || option_inner(self.types, id).is_some(),
		}
	}
	fn is_equal(&self, id: TypeId, other: TypeId, kind: EqKind) -> Tokens {
		match kind {
			EqKind::Bool => self.item(id, "is_equal_bool"),
			_ => self.item(id, format!("is_equal_{}", other.raw())),
		}
	}
	fn literal(&self, value: &Value, ty: TypeId) -> Tokens {
		let wide = matches!(
			ty.is_valid().then(|| scalar_of(self.types, ty)).flatten(),
			Some(Scalar::I64 | Scalar::U64)
		);
		let int = |n: String| -> Tokens {
			if wide {
				quote!($(format!("{n}n")))
			} else {
				quote!($n)
			}
		};
		match value {
			Value::Signed(n) => int(n.to_string()),
			Value::Unsigned(n) => int(n.to_string()),
			Value::Bool(b) => quote!($(b.to_string())),
			Value::String(s) => quote!($(quoted(s.as_str()))),
			Value::UnionDescriminator(id) => {
				quote!({ kind: $(quoted(sanitize(id.as_str()))) })
			}
		}
	}
}

impl Names<JavaScript> for TsMeta<'_> {
	fn ty(&self, id: TypeId) -> Tokens {
		self.inline_ty(id).unwrap_or_else(|| self.item(id, "Ty"))
	}
	fn serialize(&self, id: TypeId) -> Tokens {
		self.item(id, "serialize")
	}
	fn deserialize(&self, id: TypeId) -> Tokens {
		self.item(id, "deserialize")
	}
	fn size_of(&self, id: TypeId) -> Tokens {
		self.item(id, "size_of")
	}
	fn boxed(&self, owner: TypeId, slot: &Ident) -> bool {
		slot.as_str() == "Value"
			&& option_inner(self.types, owner).is_some_and(|inner| self.nullable(inner))
	}
	fn slot_ty(&self, owner: TypeId, slot: &Ident, target: TypeId) -> Tokens {
		let ty = Names::ty(self, target);
		if self.boxed(owner, slot) {
			quote!({ value: $ty })
		} else {
			ty
		}
	}
	fn slot_new(&self, owner: TypeId, slot: &Ident, value: Tokens) -> Tokens {
		if self.boxed(owner, slot) {
			quote!({ value: $value })
		} else {
			value
		}
	}
	fn scalar(&self, id: TypeId) -> Option<Scalar> {
		scalar_of(self.types, id)
	}
}

fn read_reference(meta: &TsMeta, from_locals: bool, input: &Field) -> Tokens {
	let mut expr: Tokens = quote!(_0);
	for (i, item) in input.0.iter().enumerate() {
		expr = match &item.segment {
			FieldRefSegment::Arg(name) => quote!(args.$(name.as_str())),
			FieldRefSegment::Down { to, .. } => {
				if i == 0 && from_locals {
					quote!($(format!("f_{}", to.as_str())))
				} else {
					quote!($expr.$(to.as_str()))
				}
			}
			FieldRefSegment::Virtual(name) => {
				let f = meta.item(item.from.get(), format!("virtual_{}", name.as_str()));
				quote!($f($expr))
			}
		};
	}
	expr
}

fn read_or_const_reference(
	meta: &TsMeta,
	from_locals: bool,
	input: &FieldOrConst,
	param: TypeId,
) -> Tokens {
	match input {
		FieldOrConst::Field(field) => read_reference(meta, from_locals, field),
		FieldOrConst::Const(value) => meta.literal(value, param),
	}
}

fn args_passing(meta: &TsMeta, input: &Type) -> Tokens {
	let params = &meta.types[input.target].params;
	let args: Vec<Tokens> = input
		.args
		.iter()
		.map(|(name, field)| {
			let param = params
				.iter()
				.find(|(n, _)| n == name)
				.map(|(_, t)| *t)
				.unwrap_or_default();
			quote!($(name.as_str()): $(read_or_const_reference(meta, true, field, param)))
		})
		.collect();
	quote!({ $(for a in args join(, ) => $a) })
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()> {
	let meta = TsMeta::new(input, reg);
	let unions_as_tag = common::unions_as_tag(input);
	let tag_referenced = common::tag_referenced(input);
	let eq_impls = common::eq_impls(input);

	let pinput = &js::import("../pds.ts", "Input");
	let poutput = &js::import("../pds.ts", "Output");
	let perror = &js::import("../pds.ts", "PdsError");
	let pequal = &js::import("../pds.ts", "equal");

	let mut modules = Vec::new();
	for (id, ty) in &input.0 {
		let has_args = !ty.params.is_empty();
		let param_ty = |par: TypeId| -> Tokens {
			if par.is_valid() {
				Names::ty(&meta, par)
			} else {
				quote!(number)
			}
		};
		let args_struct = if has_args {
			quote! {
				export type Args = {
					$(for (name, par) in ty.params.iter() join($['\r']) => $(name.as_str()): $(param_ty(*par));)
				};
			}
		} else {
			quote!()
		};
		let args_param = || -> Tokens {
			if has_args {
				quote!(args: Args)
			} else {
				quote!()
			}
		};
		let args_of = |target: TypeId, use_: &Type| -> Tokens {
			if input[target].params.is_empty() {
				quote!()
			} else {
				quote!(, $(args_passing(&meta, use_)))
			}
		};
		let assert_tag_call = |siblings: &[mir::ContainerField],
		                       field: &mir::ContainerField|
		 -> Tokens {
			if field.load_from.is_some() {
				return quote!();
			}
			let TypeKind::Union {
				tag: mir::UnionTag::External(tagref),
				matches,
				default,
				..
			} = &input[field.ty.target].kind
			else {
				return quote!();
			};
			if matches.is_empty() && default.is_none() {
				return quote!();
			}
			let Some(FieldRefSegment::Arg(tag_param)) = tagref.0.first().map(|i| &i.segment) else {
				return quote!();
			};
			let Some((_, val)) = field.ty.args.iter().find(|(n, _)| n == tag_param) else {
				return quote!();
			};
			if let FieldOrConst::Field(f) = val {
				match f.0.first().map(|i| &i.segment) {
					Some(FieldRefSegment::Arg(_)) => return quote!(),
					Some(FieldRefSegment::Down { to, .. })
						if siblings
							.iter()
							.any(|s| &s.name == to && s.load_from.is_some()) =>
					{
						return quote!()
					}
					_ => {}
				}
			}
			quote! {
				$(meta.item(field.ty.target, "assert_tag"))(_0.$(field.name.as_str()), $(read_or_const_reference(&meta, false, val, TypeId::default())));
			}
		};

		let body: Tokens = match &ty.kind {
			TypeKind::Container { fields, .. } => {
				let stored: Vec<&mir::ContainerField> =
					fields.iter().filter(|f| f.load_from.is_none()).collect();
				let value_of = |field: &mir::ContainerField| -> Tokens {
					match &field.load_from {
						None => quote!(_0.$(field.name.as_str())),
						Some(from) => read_reference(&meta, false, from),
					}
				};
				quote! {
					$args_struct
					export type Ty = {
						$(for f in &stored join($['\r']) => $(f.name.as_str()): $(Names::ty(&meta, f.ty.target));)
					};
					export function serialize(_0: Ty, output: $poutput): void {
						$(for f in fields.iter() join($['\r']) =>
							$(assert_tag_call(fields, f))
							$(meta.item(f.ty.target, "serialize"))($(value_of(f)), output);
						)
					}
					export function deserialize(input: $pinput$(if has_args => , $(args_param()))): Ty {
						$(for f in fields.iter() join($['\r']) =>
							const $(format!("f_{}", f.name.as_str())) = $(meta.item(f.ty.target, "deserialize"))(input$(args_of(f.ty.target, &f.ty)));
						)
						$(for f in fields.iter().filter(|f| f.load_from.is_some()) join($['\r']) =>
							if (!$pequal($(format!("f_{}", f.name.as_str())), $(read_reference(&meta, true, f.load_from.as_ref().unwrap())))) {
								throw new $perror("InvalidValue", $(quoted(format!("virtual field {} does not match its value source", f.name.as_str()))));
							}
						)
						return {
							$(for f in &stored join($['\r']) => $(f.name.as_str()): $(format!("f_{}", f.name.as_str())),)
						};
					}
					export function size_of(_0: Ty): number {
						return 0$(for f in fields.iter() => $[' ']+ $(meta.item(f.ty.target, "size_of"))($(value_of(f))));
					}
				}
			}
			TypeKind::Union {
				tag,
				tag_type,
				matches,
				default,
			} => {
				let is_inline = matches!(tag, mir::UnionTag::Inline(_));
				let is_empty = matches.is_empty() && default.is_none();
				let tag_ty = *tag_type;
				let mut tag_access = quote!(__tag);
				let mut tag_inner = tag_ty;
				while let TypeKind::Container {
					fields,
					delegate_all: Some(d),
					..
				} = &input[tag_inner].kind
				{
					let f = fields
						.iter()
						.find(|f| &f.name == d)
						.expect("delegate target exists");
					tag_access = quote!($tag_access.$(d.as_str()));
					tag_inner = f.ty.target;
				}
				let tag_access = &tag_access;
				let cond = |value: &Value| -> Tokens {
					if let Some((b, kind)) = eq_target(input, tag_inner, value) {
						let call = meta.is_equal(tag_inner, b, kind);
						return quote!($call($tag_access, $(meta.literal(value, b))));
					}
					panic!(
						"union arm value {value:?} cannot match tag type {:?}",
						meta.raw_names[&tag_inner]
					)
				};
				let tag_value = |value: &Value| -> Tokens {
					match common::tag_repr(input, tag_ty, value) {
						Some(TagRepr::Literal) => meta.literal(value, tag_ty),
						Some(TagRepr::OptionSome(opt)) => {
							let inner = option_inner(input, opt).unwrap_or_default();
							meta.slot_new(opt, &Ident::new("Value"), meta.literal(value, inner))
						}
						Some(TagRepr::Variant(_, n)) => quote!({ kind: $(quoted(n.as_str())) }),
						None => {
							quote!((() => { throw new $perror("InvalidValue", "reconstructing a non-scalar tag"); })())
						}
					}
				};
				let read_tag = match tag {
					mir::UnionTag::External(f) => {
						let tag_param = match f.0.first().map(|i| &i.segment) {
							Some(FieldRefSegment::Arg(name)) => name.clone(),
							other => anyhow::bail!(
								"union tag should be an argument by codegen: {other:?} in {:?}",
								ty.name
							),
						};
						quote!(const __tag = args.$(tag_param.as_str());)
					}
					mir::UnionTag::Inline(t) => quote! {
						const __tag = $(meta.item(t.target, "deserialize"))(input$(args_of(t.target, t)));
					},
				};
				let write_tag = match tag {
					mir::UnionTag::External(_) => quote!(),
					mir::UnionTag::Inline(t) => quote! {
						$(meta.item(t.target, "serialize"))(virtual_tag(_0), output);
					},
				};
				let tag_own_size = match tag {
					mir::UnionTag::External(_) => quote!(0),
					mir::UnionTag::Inline(t) => {
						quote!($(meta.item(t.target, "size_of"))(virtual_tag(_0)))
					}
				};

				let mut variants: Vec<Tokens> = Vec::new();
				let mut ser: Vec<Tokens> = Vec::new();
				let mut size: Vec<Tokens> = Vec::new();
				let mut vtag: Vec<Tokens> = Vec::new();
				let mut asserts: Vec<Tokens> = Vec::new();
				let mut de: Vec<Tokens> = Vec::new();
				for (value, variant, vty) in matches {
					let kind: Tokens = quote!($(quoted(variant.as_str())));
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
						variants.push(
							quote!({ kind: $(&kind); value: $(Names::ty(&meta, vty.target)) }),
						);
						ser.push(
							quote!(case $(&kind): $(meta.item(vty.target, "serialize"))(_0.value, output); return;),
						);
						size.push(
							quote!(case $(&kind): return __n + $(meta.item(vty.target, "size_of"))(_0.value);),
						);
						de.push(quote! {
							if ($(cond(value))) {
								return { kind: $(&kind), value: $(meta.item(vty.target, "deserialize"))(input$(args_of(vty.target, vty))) };
							}
						});
					}
					vtag.push(quote!(case $(&kind): return $(tag_value(value));));
					asserts.push(quote!(case $(&kind): ok = $(cond(value)); break;));
				}
				let de_default = if let Some((dname, dty)) = default {
					let kind: Tokens = quote!($(quoted(dname.as_str())));
					if input.is_void(dty.target) {
						variants.push(quote!({ kind: $(&kind); tag: $(Names::ty(&meta, tag_ty)) }));
						ser.push(quote!(case $(&kind): return;));
						size.push(quote!(case $(&kind): return __n;));
						vtag.push(quote!(case $(&kind): return _0.tag;));
						quote!(return { kind: $(&kind), tag: __tag };)
					} else {
						variants.push(quote!({ kind: $(&kind); tag: $(Names::ty(&meta, tag_ty)); value: $(Names::ty(&meta, dty.target)) }));
						ser.push(
							quote!(case $(&kind): $(meta.item(dty.target, "serialize"))(_0.value, output); return;),
						);
						size.push(
							quote!(case $(&kind): return __n + $(meta.item(dty.target, "size_of"))(_0.value);),
						);
						vtag.push(quote!(case $(&kind): return _0.tag;));
						quote!(return { kind: $(&kind), tag: __tag, value: $(meta.item(dty.target, "deserialize"))(input$(args_of(dty.target, dty))) };)
					}
				} else {
					quote!(throw new $perror("InvalidValue", "no union arm matches the tag");)
				};

				let my_eq = eq_impls.get(id).cloned().unwrap_or_default();
				let eq_fns: Vec<Tokens> = my_eq
					.iter()
					.map(|(b, kind)| match kind {
						EqKind::SelfEq => quote! {
							export function $(format!("is_equal_{}", b.raw()))(a: Ty, b: Ty): boolean {
								return a.kind === b.kind;
							}
						},
						EqKind::VirtualTag => quote! {
							export function $(format!("is_equal_{}", b.raw()))(a: Ty, b: $(Names::ty(&meta, *b))): boolean {
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

				let expose_tag =
					is_inline || unions_as_tag.contains(id) || tag_referenced.contains(id);
				if is_empty {
					quote! {
						$args_struct
						export type Ty = never;
						export function serialize(_0: Ty, _output: $poutput): void {
							throw new $perror("InvalidValue", "empty union");
						}
						export function deserialize(input: $pinput$(if has_args => , $(args_param()))): Ty {
							$read_tag
							throw new $perror("InvalidValue", "empty union");
						}
						export function size_of(_0: Ty): number {
							throw new $perror("InvalidValue", "empty union");
						}
						$(if expose_tag =>
							export function virtual_tag(_0: Ty): $(Names::ty(&meta, tag_ty)) {
								throw new $perror("InvalidValue", "empty union");
							}
						)
						$(for f in &eq_fns join($['\r']) => $f)
					}
				} else {
					quote! {
						$args_struct
						export type Ty =
							$(for v in &variants join($['\r']) => | $v)
						;
						export function serialize(_0: Ty, output: $poutput): void {
							$write_tag
							switch (_0.kind) {
								$(for a in &ser join($['\r']) => $a)
							}
						}
						export function deserialize(input: $pinput$(if has_args => , $(args_param()))): Ty {
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
							export function assert_tag(_0: Ty, __tag: $(Names::ty(&meta, tag_ty))): void {
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
							export function virtual_tag(_0: Ty): $(Names::ty(&meta, tag_ty)) {
								switch (_0.kind) {
									$(for a in &vtag join($['\r']) => $a)
								}
							}
						)
						$(for f in &eq_fns join($['\r']) => $f)
					}
				}
			}
			TypeKind::Alias(target) => {
				quote! {
					export type Ty = $(Names::ty(&meta, target.target));
					export function serialize(_0: Ty, output: $poutput): void {
						$(meta.item(target.target, "serialize"))(_0, output);
					}
					export function deserialize(input: $pinput): Ty {
						return $(meta.item(target.target, "deserialize"))(input$(args_of(target.target, target)));
					}
					export function size_of(_0: Ty): number {
						return $(meta.item(target.target, "size_of"))(_0);
					}
				}
			}
			TypeKind::Void => {
				quote! {
					export type Ty = null;
					export function serialize(_0: Ty, _output: $poutput): void {}
					export function deserialize(_input: $pinput): Ty {
						return null;
					}
					export function size_of(_0: Ty): number {
						return 0;
					}
				}
			}
			TypeKind::Native(native) => {
				let eq_entries: Vec<(TypeId, EqKind)> = eq_impls
					.get(id)
					.map(|s| s.iter().copied().collect())
					.unwrap_or_default();
				let eq_ids: Vec<TypeId> = eq_entries
					.iter()
					.filter(|(_, k)| *k != EqKind::Bool)
					.map(|(b, _)| *b)
					.collect();
				let bool_eq = eq_entries.iter().any(|(_, k)| *k == EqKind::Bool);
				let bool_fn = quote! {
					$(if bool_eq =>
						export function is_equal_bool(a: Ty, b: boolean): boolean {
							return a === b;
						}
					)
				};
				match &native.specializer {
					Some((specializer, variant)) => {
						let resolve = |n: &str| {
							meta.raw_names
								.iter()
								.find(|(_, v)| v.as_str() == n)
								.map(|(k, _)| *k)
						};
						let Some(codegen) = reg.codegen::<JavaScript>(specializer.as_str()) else {
							anyhow::bail!("no typescript codegen for specializer {specializer:?}");
						};
						let args_struct_gen = || args_struct.clone();
						let pass_args = |u: &Type| -> Tokens {
							if input[u.target].params.is_empty() {
								quote!()
							} else {
								args_passing(&meta, u)
							}
						};
						let body = codegen
							.generate(&NativeCodegenCtx {
								id: *id,
								variant,
								native,
								params: &ty.params,
								names: &meta,
								resolve: &resolve,
								args_struct: &args_struct_gen,
								args_param: &args_param,
								pass_args: &pass_args,
								eq_targets: &eq_ids,
							})
							.with_context(|| format!("typescript codegen for {:?}", ty.name))?;
						quote! {
							$body
							$bool_fn
						}
					}
					None => {
						let native_import = &js::import("../pds_native.ts", meta.name_str(*id));
						quote! {
							export type Ty = ReturnType<typeof $native_import.deserialize>;
							export const serialize = $native_import.serialize;
							export const deserialize = $native_import.deserialize;
							export const size_of = $native_import.size_of;
							$(for (b, kind) in &eq_entries join($['\r']) =>
								$(if *kind != EqKind::Bool =>
									export function $(format!("is_equal_{}", b.raw()))(a: Ty, b: Ty): boolean {
										return a === b;
									}
								)
							)
							$bool_fn
						}
					}
				}
			}
		};
		output.insert(
			format!("{}.ts", meta.name_str(*id)).into(),
			body.to_file_string().expect("formatting should succeed"),
		);
		modules.push(meta.name_str(*id));
	}

	let index: Tokens = quote! {
		$(for m in &modules join($['\r']) => export * as $m from $(quoted(format!("./{m}.ts")));)
	};
	output.insert(
		"mod.ts".into(),
		index.to_file_string().expect("formatting should succeed"),
	);
	Ok(())
}
