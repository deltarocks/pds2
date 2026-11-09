use genco::lang::rust::import;
use genco::prelude::*;
use genco::quote;
use rust::{Import, Tokens};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::ast::{FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::FieldOrConst;
use crate::mir::{self, Field, Type, TypeKind, TypeMap, Value};

use super::boxing::{self, Boxed};
use super::common::{self, eq_target, sanitize, scalar_of, EqKind, TagRepr};
use super::GeneratorOutput;
use crate::specializer::{Names, NativeCodegenCtx, Registry, Scalar};
use anyhow::{Context as _, Result};
use genco::lang::Rust;

#[derive(Clone)]
struct RustMeta<'a> {
	raw_names: Rc<BTreeMap<TypeId, String>>,
	types: &'a TypeMap,
	reg: &'a Registry,
	boxed: Rc<Boxed>,
	inlining: Rc<RefCell<BTreeSet<TypeId>>>,
}
impl<'a> RustMeta<'a> {
	fn new(source: &'a TypeMap, reg: &'a Registry, boxed: Boxed) -> Self {
		let mut raw_names = BTreeMap::new();
		for (id, ty) in source.0.iter() {
			raw_names.insert(*id, ty.name.readable_name());
		}
		Self {
			raw_names: Rc::new(raw_names),
			types: source,
			reg,
			boxed: Rc::new(boxed),
			inlining: Rc::default(),
		}
	}
	fn inline_ty(&self, type_id: TypeId) -> Option<Tokens> {
		let TypeKind::Native(native) = &self.types[type_id].kind else {
			return None;
		};
		let (specializer, _) = native.specializer.as_ref()?;
		let codegen = self.reg.codegen::<Rust>(specializer.as_str())?;
		if !self.inlining.borrow_mut().insert(type_id) {
			return None;
		}
		let ty = codegen.inline_ty(type_id, native, self);
		self.inlining.borrow_mut().remove(&type_id);
		ty
	}
	fn name_str(&self, type_id: TypeId) -> String {
		assert!(type_id.is_valid());
		format!("pds_{}", self.raw_names[&type_id])
	}
	fn name(&self, type_id: TypeId) -> Tokens {
		assert!(type_id.is_valid());
		quote!(pds_$(&self.raw_names[&type_id]))
	}
	fn imported(&self, type_id: TypeId, ty: impl AsRef<str>) -> Import {
		import(format!("super::{}", self.name_str(type_id)), ty.as_ref()).qualified()
	}
	fn ty(&self, type_id: TypeId) -> Tokens {
		assert!(type_id.is_valid());
		match self.inline_ty(type_id) {
			Some(ty) => ty,
			None => quote!($(self.imported(type_id, "Ty"))),
		}
	}
	fn virt(&self, type_id: TypeId, field: Ident) -> Import {
		self.imported(type_id, format!("virtual_{}", field.as_str()))
	}
	/// `is_equal_{other}` on `type_id`'s module: compares a value of this type
	/// against a value of `other` (used for union arm matching on non-scalar tags
	/// like `option`). For `EqKind::Bool` constants the helper is named
	/// `is_equal_bool` (keyed by value kind, not by tag type id).
	fn is_equal(&self, type_id: TypeId, other: TypeId, kind: EqKind) -> Import {
		let name = match kind {
			EqKind::Bool => "is_equal_bool".to_string(),
			_ => format!("is_equal_{}", other.raw()),
		};
		self.imported(type_id, name)
	}
	fn args(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "Args")
	}
	fn serialize(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "serialize")
	}
	fn deserialize(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "deserialize")
	}
	fn size_of(&self, type_id: TypeId) -> Import {
		self.imported(type_id, "size_of")
	}
}

impl Names<Rust> for RustMeta<'_> {
	fn ty(&self, id: TypeId) -> Tokens {
		RustMeta::ty(self, id)
	}
	fn serialize(&self, id: TypeId) -> Tokens {
		quote!($(RustMeta::serialize(self, id)))
	}
	fn deserialize(&self, id: TypeId) -> Tokens {
		quote!($(RustMeta::deserialize(self, id)))
	}
	fn size_of(&self, id: TypeId) -> Tokens {
		quote!($(RustMeta::size_of(self, id)))
	}
	fn boxed(&self, owner: TypeId, slot: &Ident) -> bool {
		self.boxed.contains(&(owner, slot.clone()))
	}
	fn slot_ty(&self, owner: TypeId, slot: &Ident, target: TypeId) -> Tokens {
		let ty = RustMeta::ty(self, target);
		if Names::boxed(self, owner, slot) {
			quote!(Box<$ty>)
		} else {
			ty
		}
	}
	fn slot_new(&self, owner: TypeId, slot: &Ident, value: Tokens) -> Tokens {
		if Names::boxed(self, owner, slot) {
			quote!(Box::new($value))
		} else {
			value
		}
	}
	fn scalar(&self, id: TypeId) -> Option<Scalar> {
		scalar_of(self.types, id)
	}
}

/// Emit an identifier, raw-prefixed when it collides with a rust keyword
/// (protodef loves fields named `type`).
fn rname(i: &Ident) -> Tokens {
	const KEYWORDS: &[&str] = &[
		"as", "async", "await", "box", "break", "const", "continue", "crate", "do", "dyn", "else",
		"enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
		"move", "mut", "pub", "ref", "return", "static", "struct", "trait", "true", "type",
		"unsafe", "use", "where", "while", "yield", "abstract", "become", "final", "macro",
		"override", "priv", "try", "typeof", "unsized", "virtual",
	];
	if KEYWORDS.contains(&i.as_str()) {
		quote!($(format!("r#{}", i.as_str())))
	} else {
		quote!($i)
	}
}

fn generate_read_or_const_reference(
	types: &RustMeta,
	from_locals: bool,
	input: &FieldOrConst,
	target: Option<&Tokens>,
) -> Tokens {
	let input = match input {
		FieldOrConst::Field(field) => field,
		FieldOrConst::Const(mir::Value::Signed(n)) => {
			return quote!(&($(n.to_string())).try_into().expect("constant fits"))
		}
		FieldOrConst::Const(mir::Value::Unsigned(n)) => {
			return quote!(&($(n.to_string())).try_into().expect("constant fits"))
		}
		FieldOrConst::Const(mir::Value::Bool(b)) => return quote!(&$(b.to_string())),
		FieldOrConst::Const(mir::Value::String(s)) => {
			return quote!(&$(quoted(s.as_str())).to_owned())
		}
		FieldOrConst::Const(mir::Value::UnionDescriminator(id)) => {
			unreachable!("union discriminator constant {id:?} as an argument")
		}
	};
	generate_read_reference(types, from_locals, input, target)
}
fn generate_read_reference(
	types: &RustMeta,
	from_locals: bool,
	input: &Field,
	target: Option<&Tokens>,
) -> Tokens {
	let mut indexing = vec![];
	let computed = input
		.0
		.iter()
		.any(|f| matches!(f.segment, FieldRefSegment::Virtual(_)));
	for (i, field) in input.0.iter().enumerate() {
		indexing.push(match &field.segment {
			FieldRefSegment::Arg(name) => {
				assert_eq!(i, 0);
				quote! {
					let _0 = args.$(rname(name));
				}
			}
			FieldRefSegment::Down { to, .. } => {
				if i == 0 && from_locals {
					// deserialize locals are f_-prefixed to avoid shadowing
					// `input`/`args`
					quote!(let _0: &$(types.ty(field.to.get())) = &$(format!("f_{}", to.as_str()));)
				} else {
					quote!(let _0: &$(types.ty(field.to.get())) = &_0.$(rname(to));)
				}
			}
			FieldRefSegment::Virtual(name) => {
				let local = format!("__v{i}");
				quote! {
					let $(&local): $(types.ty(field.to.get())) = $(types.virt(field.from.get(), name.clone()))(_0);
					let _0 = &$(&local);
				}
			}
		})
	}
	if let Some(target) = target {
		indexing.push(quote!(Into::<$target>::into(*_0)));
		quote!({$(for i in indexing => $i)})
	} else if computed {
		indexing.push(quote!(_0.clone()));
		quote!(&{$(for i in indexing => $i)})
	} else {
		indexing.push(quote!(_0));
		quote!({$(for i in indexing => $i)})
	}
}

fn generate_args_passing(types: &RustMeta, input: &Type) -> Tokens {
	let mut args = vec![];
	for (name, field) in input.args.iter() {
		args.push(
			quote!($(rname(name)): $(generate_read_or_const_reference(types, true, field, None))),
		)
	}
	quote! {
		$(types.args(input.target)) {
			$(for arg in args join(,) => $arg)
		}
	}
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()> {
	let boxed = boxing::analyze(input, reg)?;
	let meta = RustMeta::new(input, reg, boxed);

	// Unions referenced as another union's discriminant must expose `virtual_tag`
	// even when they are external-tagged (the referencing union calls it to compute
	// the discriminant). Inline-tag unions always expose it (for their own serialize).
	let unions_as_tag = common::unions_as_tag(input);
	let tag_referenced = common::tag_referenced(input);

	// For every union arm, record which `is_equal_{b}` helpers its tag type must
	// expose. Each tag-capable module emits these from its own entry; the union
	// codegen calls them instead of inlining tag-kind-specific comparisons.
	let eq_impls = common::eq_impls(input);

	let mut out_module = vec![];

	// pi!(pi => "#![allow(non_camel_case_types, dead_code, unused_imports, unused_parens, clippy::needless_borrow, clippy::just_underscores_and_digits, clippy::identity_op)]" NL);
	// pi!(pi => "use crate::pds::{Input, Output, Result};" NL);
	for (id, ty) in &input.0 {
		let pinput = &import("crate::pds", "Input");
		let poutput = &import("crate::pds", "Output");
		let presult = &import("crate::pds", "Result");
		let _name = ty.name.readable_name();
		let has_args = !ty.params.is_empty();
		// Params with no inferred type (only ever bound to constants) fall back to a
		// plain integer.
		let param_ty = |par: TypeId| -> Tokens {
			if par.is_valid() {
				quote!($(meta.ty(par)))
			} else {
				quote!(usize)
			}
		};
		let args_struct = quote! {
			$(if has_args =>
				pub struct Args<'a> {
					$(for (name, par) in ty.params.iter() join($['\r']) => pub $(rname(name)): &'a $(param_ty(*par)),)
				}
			)
		};
		// args to pass when referencing `target` from generated code
		let args_of = |target: TypeId, ty: &Type| -> Tokens {
			if input[target].params.is_empty() {
				quote!()
			} else {
				quote!($(generate_args_passing(&meta, ty)),)
			}
		};
		// For a container field that is an external-tag union, a debug-build check that
		// the union variant agrees with the tag field it is keyed on.
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
			// The tag must be readable from `_0` alone. A tag that is itself a
			// closure-converted param (threaded only at deserialize time, e.g. a
			// bitflags field passed down through an array) is not available in
			// serialize, so skip the check there.
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
				$(meta.imported(field.ty.target, "debug_assert_tag"))(
					&_0.$(rname(&field.name)),
					$(generate_read_or_const_reference(&meta, false, val, None)),
				);
			}
		};
		let def = if !ty.kind.is_native() {
			let mod_contents = match &ty.kind {
				TypeKind::Container { fields, .. } => {
					quote! {
						$args_struct
						#[derive(Debug, Clone, PartialEq)]
						pub struct Ty {
							$(for field in fields.iter() join($['\r']) =>
								$(if field.load_from.is_none() {
									pub $(rname(&field.name)): $(meta.slot_ty(*id, &field.name, field.ty.target)),
								} else {
									$("// ") $(&field.name) $("- virtual field")
								})
							)
						}
						pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
							$(for field in fields.iter() join($['\r']) =>
								$(assert_tag_call(fields, field))
								$(meta.serialize(field.ty.target))(
									$(if field.load_from.is_none() {
										&_0.$(rname(&field.name))
									} else {
										$(generate_read_reference(&meta, false, field.load_from.as_ref().unwrap(), None))
									}),
									output,
								)?;
							)
							Ok(())
						}
						pub fn deserialize(input: &mut $pinput, $(if has_args => args: Args<'_>,)) -> $presult<Ty> {
							$(for field in fields.iter() join($['\r']) =>
								let $(format!("f_{}", field.name.as_str())) = $(meta.deserialize(field.ty.target))(
									input,
									$(args_of(field.ty.target, &field.ty))
								)?;
							)
							$(for field in fields.iter().filter(|f| f.load_from.is_some()) join($['\r']) =>
								debug_assert!(
									&$(format!("f_{}", field.name.as_str())) == $(generate_read_reference(&meta, true, field.load_from.as_ref().unwrap(), None)),
									$(quoted(format!("virtual field {} does not match its value source", field.name.as_str())))
								);
							)
							Ok(Ty {
								$(for field in fields.iter().filter(|f| f.load_from.is_none()) join(,) =>
									$(rname(&field.name)): $(meta.slot_new(*id, &field.name, quote!($(format!("f_{}", field.name.as_str())))))
								)
							})
						}
						pub fn size_of(_0: &Ty) -> usize {
							0
							$(for field in fields.iter() join($['\r']) =>
								+ $(meta.size_of(field.ty.target))(
									$(if field.load_from.is_none() {
										&_0.$(rname(&field.name))
									} else {
										$(generate_read_reference(&meta, false, field.load_from.as_ref().unwrap(), None))
									})
								)
							)
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
					// How the deserializer obtains the tag: an external tag is a
					// (closure-converted) param of this def, an inline tag is read from
					// the input right here (kept owned in `__tag` so an inline default
					// arm can store it for serialization).
					let read_tag = match tag {
						mir::UnionTag::External(f) => {
							let tag_param = match f.0.first().map(|i| &i.segment) {
								Some(FieldRefSegment::Arg(name)) => name.clone(),
								other => anyhow::bail!(
									"union tag should be an argument by codegen: {other:?} in {:?}",
									ty.name
								),
							};
							quote!(let __tag = args.$(rname(&tag_param));)
						}
						mir::UnionTag::Inline(t) => quote! {
							let __tagv = $(meta.deserialize(t.target))(
								input,
								$(args_of(t.target, t))
							)?;
							let __tag = &__tagv;
						},
					};
					// An inline-tag union writes its own tag back on serialize,
					// reconstructed from the arm via virtual_tag.
					let write_tag = match tag {
						mir::UnionTag::External(_) => quote!(),
						mir::UnionTag::Inline(t) => quote! {
							$(meta.serialize(t.target))(&virtual_tag(_0), output)?;
						},
					};
					let tag_own_size = match tag {
						mir::UnionTag::External(_) => quote!(0),
						mir::UnionTag::Inline(t) => {
							quote!($(meta.size_of(t.target))(&virtual_tag(_0)))
						}
					};
					let tag_ty = *tag_type;
					// Follow delegate_all chains (mapper unions sit inside a virt
					// wrapper struct) to the value that distinguishes arms.
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
						tag_access = quote!(&$tag_access.$(rname(d)));
						tag_inner = f.ty.target;
					}
					let tag_access = &tag_access;
					// Arm match condition, routed through the tag type's `is_equal_{b}`.
					let cond = |value: &Value| -> Tokens {
						if let Some((b_id, eq_kind)) = eq_target(input, tag_inner, value) {
							let call = meta.is_equal(tag_inner, b_id, eq_kind);
							let arg: Tokens = match value {
								Value::Signed(n) => quote!(&$(n.to_string())),
								Value::Unsigned(n) => quote!(&$(n.to_string())),
								Value::Bool(b) => quote!(&$(b.to_string())),
								Value::String(s) => quote!(&$(quoted(s.as_str())).to_owned()),
								Value::UnionDescriminator(id) => {
									let vname = Ident::new(&sanitize(id.as_str()));
									quote!(&$(meta.ty(tag_inner))::$(rname(&vname)))
								}
							};
							return quote!($call($tag_access, $arg));
						}
						// Every arm must be expressible as an `is_equal` call. Tag types are
						// value-less (mappers, enums, scalars), so this always holds. A
						// payload-carrying union used as a tag (can't reconstruct the variant)
						// or a value that can't match its tag type (malformed protocol, e.g.
						// the dropped minecraft-data `"false"` cruft) is unsupported and fails
						// loud rather than miscompiling.
						panic!(
							"union arm value {value:?} cannot match tag type {:?}",
							meta.raw_names[&tag_inner]
						)
					};
					// the tag value an arm corresponds to, for virtual_tag
					let tag_value = |value: &Value| -> Tokens {
						let literal = || -> Tokens {
							match value {
								Value::Signed(n) => quote!($(n.to_string())),
								Value::Unsigned(n) => quote!($(n.to_string())),
								Value::Bool(b) => quote!($(b.to_string())),
								Value::String(s) => quote!($(quoted(s.as_str())).into()),
								Value::UnionDescriminator(_) => quote!(unreachable!()),
							}
						};
						match common::tag_repr(input, tag_ty, value) {
							Some(TagRepr::Literal) => literal(),
							Some(TagRepr::OptionSome(opt)) => {
								let inner = meta.slot_new(opt, &Ident::new("Value"), literal());
								quote!(Some($inner))
							}
							Some(TagRepr::Variant(u, n)) => quote!($(meta.ty(u))::$(rname(&n))),
							None => quote!(todo!("reconstructing a non-scalar tag")),
						}
					};
					// Build per-arm tokens. A void-typed arm (the deduplicated canonical
					// void) becomes a value-less enum variant; an inline union's default
					// arm stores the read tag value so it can be written back.
					let void_of = |t: TypeId| input.is_void(t);
					// Box an arm payload only when it can reach this union by value
					// (closes a cycle); otherwise hold it inline.
					let boxed_of = |name: &Ident| meta.boxed(*id, name);
					let payload_ty = |t: TypeId, boxed: bool| -> Tokens {
						if boxed {
							quote!(Box<$(meta.ty(t))>)
						} else {
							quote!($(meta.ty(t)))
						}
					};
					let de_value = |target: TypeId, u: &Type, boxed: bool| -> Tokens {
						let read =
							quote!($(meta.deserialize(target))(input, $(args_of(target, u)))?);
						if boxed {
							quote!(Box::new($read))
						} else {
							read
						}
					};
					let mut variants_decl: Vec<Tokens> = Vec::new();
					let mut ser_arms: Vec<Tokens> = Vec::new();
					let mut size_arms: Vec<Tokens> = Vec::new();
					let mut vtag_arms: Vec<Tokens> = Vec::new();
					// for external unions: per-variant "does the external tag match" check
					let mut assert_arms: Vec<Tokens> = Vec::new();
					let mut de_branches: Vec<Tokens> = Vec::new();
					for (value, variant, vty) in matches {
						let v = rname(variant);
						if void_of(vty.target) {
							variants_decl.push(quote!($(&v),));
							ser_arms.push(quote!(Ty::$(&v) => Ok(()),));
							size_arms.push(quote!(Ty::$(&v) => 0,));
							vtag_arms.push(quote!(Ty::$(&v) => $(tag_value(value)),));
							assert_arms.push(quote!(Ty::$(&v) => $(cond(value)),));
							de_branches.push(quote!(if $(cond(value)) { return Ok(Ty::$(&v)); }));
						} else {
							let boxed = boxed_of(variant);
							variants_decl.push(quote!($(&v)($(payload_ty(vty.target, boxed))),));
							ser_arms.push(
								quote!(Ty::$(&v)(__v) => $(meta.serialize(vty.target))(__v, output),),
							);
							size_arms
								.push(quote!(Ty::$(&v)(__v) => $(meta.size_of(vty.target))(__v),));
							vtag_arms.push(quote!(Ty::$(&v)(_) => $(tag_value(value)),));
							assert_arms.push(quote!(Ty::$(&v)(_) => $(cond(value)),));
							de_branches.push(quote! {
								if $(cond(value)) {
									return Ok(Ty::$(&v)($(de_value(vty.target, vty, boxed))));
								}
							});
						}
					}
					let de_default: Tokens = if let Some((dname, dty)) = default.as_ref() {
						let d = rname(dname);
						let void = void_of(dty.target);
						let boxed = boxed_of(dname);
						let dpay = de_value(dty.target, dty, boxed);
						let tag_owned = if is_inline {
							quote!(__tagv)
						} else {
							quote!(__tag.clone())
						};
						if void {
							variants_decl.push(quote!($(&d)($(meta.ty(tag_ty))),));
							ser_arms.push(quote!(Ty::$(&d)(_) => Ok(()),));
							size_arms.push(quote!(Ty::$(&d)(_) => 0,));
							vtag_arms.push(quote!(Ty::$(&d)(__t) => __t.clone(),));
							assert_arms.push(quote!(Ty::$(&d)(_) => true,));
							quote!(Ok(Ty::$(&d)($tag_owned)))
						} else {
							variants_decl.push(
								quote!($(&d)($(meta.ty(tag_ty)), $(payload_ty(dty.target, boxed))),),
							);
							ser_arms.push(
								quote!(Ty::$(&d)(_, __v) => $(meta.serialize(dty.target))(__v, output),),
							);
							size_arms.push(
								quote!(Ty::$(&d)(_, __v) => $(meta.size_of(dty.target))(__v),),
							);
							vtag_arms.push(quote!(Ty::$(&d)(__t, _) => __t.clone(),));
							assert_arms.push(quote!(Ty::$(&d)(..) => true,));
							quote!(Ok(Ty::$(&d)($tag_owned, $dpay)))
						}
					} else {
						quote!(Err($(import("crate::pds", "Error"))::InvalidValue))
					};
					// an empty (uninhabited) enum must be matched by value; its bodies are
					// unreachable, and `!` does not coerce as an `Add`/argument operand.
					let is_empty = matches.is_empty() && default.is_none();
					let match_head = if is_empty {
						quote!(match *_0)
					} else {
						quote!(match _0)
					};
					let match_head = &match_head;
					// `is_equal_{b}` helpers this union's module must expose because it is used
					// as another union's discriminant (see `eq_impls`).
					let my_eq = eq_impls.get(id).cloned().unwrap_or_default();
					let derive_attr = quote!(#[derive(Debug, Clone, PartialEq)]);
					let eq_fns: Vec<Tokens> = my_eq
						.iter()
						.map(|(b, kind)| match kind {
							EqKind::SelfEq => quote! {
								pub fn $(format!("is_equal_{}", b.raw()))(a: &Ty, b: &Ty) -> bool { a == b }
							},
							EqKind::VirtualTag => quote! {
								pub fn $(format!("is_equal_{}", b.raw()))(a: &Ty, b: &$(meta.ty(*b))) -> bool {
									&virtual_tag(a) == b
								}
							},
							EqKind::OptionInner => quote!(),
							EqKind::Bool => quote! {
								pub fn is_equal_bool(a: &Ty, b: &bool) -> bool { a == b }
							},
						})
						.collect();
					quote! {
						$args_struct
						$derive_attr
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
						pub fn deserialize(input: &mut $pinput, $(if has_args => args: Args<'_>,)) -> $presult<Ty> {
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
							pub fn debug_assert_tag(_0: &Ty, __tag: &$(meta.ty(tag_ty))) {
								debug_assert!(
									$match_head {
										$(for a in &assert_arms join($['\r']) => $a)
									},
									"union variant does not match its external tag",
								);
							}
						)
						$(if is_inline || unions_as_tag.contains(id) || tag_referenced.contains(id) =>
							pub fn virtual_tag(_0: &Ty) -> $(meta.ty(tag_ty)) {
								$(if is_empty => match *_0 {})
								$(if !is_empty {
									$match_head {
										$(for a in &vtag_arms join($['\r']) => $a)
									}
								})
							}
						)
						$(for f in &eq_fns join($['\r']) => $f)
					}
				}
				TypeKind::Alias(target) => {
					quote! {
						pub type Ty = $(meta.ty(target.target));
						pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
							$(meta.serialize(target.target))(_0, output)
						}
						pub fn deserialize(input: &mut $pinput) -> $presult<Ty> {
							$(meta.deserialize(target.target))(
								input,
								$(args_of(target.target, target))
							)
						}
						pub fn size_of(_0: &Ty) -> usize {
							$(meta.size_of(target.target))(_0)
						}
					}
				}
				TypeKind::Void => {
					quote! {
						pub type Ty = ();
						pub fn serialize(_0: &Ty, _output: &mut $poutput) -> $presult<()> {
							Ok(())
						}
						pub fn deserialize(_input: &mut $pinput) -> $presult<Ty> {
							Ok(())
						}
						pub fn size_of(_0: &Ty) -> usize {
							0
						}
					}
				}
				TypeKind::Native(_) => unreachable!("handled by the native branch"),
			};
			mod_contents
		} else {
			let TypeKind::Native(native) = &ty.kind else {
				unreachable!();
			};
			let eq_entries: Vec<(TypeId, EqKind)> =
				eq_impls.get(id).map(|s| s.iter().copied().collect()).unwrap_or_default();
			// Specializer natives (e.g. `option`) consume per-typeid eq helpers; the
			// kind-named `Bool` helper is emitted inline below and does not flow
			// through the specializer interface.
			let eq_ids: Vec<TypeId> = eq_entries
				.iter()
				.filter(|(_, k)| *k != EqKind::Bool)
				.map(|(b, _)| *b)
				.collect();
			match native {
				mir::NativeType {
					specializer: Some((specializer, variant)),
					..
				} => {
					let resolve = |n: &str| {
						meta.raw_names
							.iter()
							.find(|(_, v)| v.as_str() == n)
							.map(|(k, _)| *k)
					};
					let Some(codegen) = reg.codegen::<Rust>(specializer.as_str()) else {
						anyhow::bail!("no rust codegen for specializer {specializer:?}");
					};
					let args_struct_gen = || args_struct.clone();
					let args_param = || {
						if has_args {
							quote!(args: Args<'_>,)
						} else {
							quote!()
						}
					};
					let pass_args = |u: &Type| args_of(u.target, u);
					codegen
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
						.with_context(|| format!("codegen for {:?}", ty.name))?
				}
				_ => {
					let import = import(
						"crate::pds_native",
						format!("pds_{}", ty.name.readable_name()),
					);
					// Scalar leaves used as a union discriminant compare by value.
					let eq_fns: Vec<Tokens> = eq_entries
						.iter()
						.map(|(b, kind)| match kind {
							EqKind::Bool => quote! {
								pub fn is_equal_bool(a: &Ty, b: &bool) -> bool { a == b }
							},
							_ => quote! {
								pub fn $(format!("is_equal_{}", b.raw()))(a: &Ty, b: &Ty) -> bool { a == b }
							},
						})
						.collect();
					quote! {
						pub use $import::*;
						$(for f in &eq_fns join($['\r']) => $f)
					}
				}
			}
		};
		output.insert(
			format!("{}.rs", meta.name_str(*id)).into(),
			def.to_file_string().expect("formatting should succeed"),
		);
		out_module.push(quote! {
			pub mod $(meta.name(*id));
		})
	}

	output.insert(
		"mod.rs".into(),
		quote! {
			$(for m in out_module join($['\r']) => $m)
		}
		.to_file_string()
		.expect("formatting should succeed"),
	);
	Ok(())
}
