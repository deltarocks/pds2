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

use super::GeneratorOutput;
use crate::specializer::{NativeCodegenCtx, Registry, RustNames};
use anyhow::{Context as _, Result};

#[derive(Clone)]
struct RustMeta<'a> {
	raw_names: Rc<BTreeMap<TypeId, String>>,
	types: &'a TypeMap,
	reg: &'a Registry,
	inlining: Rc<RefCell<BTreeSet<TypeId>>>,
}
impl<'a> RustMeta<'a> {
	fn new(source: &'a TypeMap, reg: &'a Registry) -> Self {
		let mut raw_names = BTreeMap::new();
		for (id, ty) in source.0.iter() {
			raw_names.insert(*id, ty.name.readable_name());
		}
		Self {
			raw_names: Rc::new(raw_names),
			types: source,
			reg,
			inlining: Rc::default(),
		}
	}
	fn inline_ty(&self, type_id: TypeId) -> Option<Tokens> {
		let TypeKind::Native(native) = &self.types[type_id].kind else {
			return None;
		};
		let (specializer, _) = native.specializer.as_ref()?;
		let codegen = self.reg.codegen::<Tokens>(specializer.as_str())?;
		if !self.inlining.borrow_mut().insert(type_id) {
			return None;
		}
		let ty = codegen.inline_ty(native, self);
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

impl RustNames for RustMeta<'_> {
	fn ty(&self, id: TypeId) -> Tokens {
		RustMeta::ty(self, id)
	}
	fn serialize(&self, id: TypeId) -> Import {
		RustMeta::serialize(self, id)
	}
	fn deserialize(&self, id: TypeId) -> Import {
		RustMeta::deserialize(self, id)
	}
	fn size_of(&self, id: TypeId) -> Import {
		RustMeta::size_of(self, id)
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

/// Sanitize an arbitrary string (`minecraft:foo`) into an identifier, mirroring the
/// upgrade-side variant naming.
fn sanitize(key: &str) -> String {
	let mut out: String = key
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() || c == '_' {
				c
			} else {
				'_'
			}
		})
		.collect();
	if out.is_empty() {
		out.push('_');
	}
	if out
		.chars()
		.next()
		.map(|c| c.is_ascii_digit())
		.unwrap_or(false)
	{
		out.insert(0, 'v');
	}
	out
}

/// How a tag type's generated `is_equal_{b}(a: &Ty, b: &b::Ty)` compares an arm
/// value against the runtime tag. The function lives on the *tag* type's module
/// so the union codegen stays closed over the set of tag kinds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum EqKind {
	/// `a == b` — tag and arm value share the type (requires `PartialEq`); used by
	/// scalar leaves (int/bool), strings, and value-less tag enums.
	SelfEq,
	/// `&virtual_tag(a) == b` — a union tag matched by its wire scalar.
	VirtualTag,
	/// `a.as_deref() == Some(b)` — an `option` tag matched by its inner value.
	OptionInner,
	/// `a == b` against a bool literal; emitted as `is_equal_bool(a: &Ty, b: &bool)`,
	/// named by value kind rather than tag type id so the codegen carries no
	/// bool-special-case lookup.
	Bool,
}

/// The inner value type of an `option` native, if `id` is one.
fn option_inner(input: &TypeMap, id: TypeId) -> Option<TypeId> {
	if let TypeKind::Native(nt) = &input[id].kind {
		if matches!(&nt.specializer, Some((s, _)) if s.as_str() == "option") {
			return nt
				.specialization
				.iter()
				.find(|(n, _)| n.as_str() == "Value")
				.map(|(_, t)| t.target);
		}
	}
	None
}

/// Follow `delegate_all` wrappers down to the type that actually distinguishes a
/// union's arms (mapper unions sit inside a virtual wrapper struct).
fn resolve_tag_inner(input: &TypeMap, tag_type: TypeId) -> TypeId {
	let mut t = tag_type;
	while let TypeKind::Container {
		fields,
		delegate_all: Some(d),
		..
	} = &input[t].kind
	{
		t = fields
			.iter()
			.find(|f| &f.name == d)
			.expect("delegate target exists")
			.ty
			.target;
	}
	t
}

/// The `(b type id, comparison kind)` an arm `value` needs from `tag_inner`'s
/// module, or `None` when the value can never match this tag type (the arm is
/// dead) — the union codegen falls back to a discriminant check or a literal
/// `false` in that case.
fn eq_target(input: &TypeMap, tag_inner: TypeId, value: &Value) -> Option<(TypeId, EqKind)> {
	let kind = &input[tag_inner].kind;
	match value {
		Value::Signed(_) | Value::Unsigned(_) => {
			if let TypeKind::Union { tag_type, .. } = kind {
				Some((*tag_type, EqKind::VirtualTag))
			} else if let Some(inner) = option_inner(input, tag_inner) {
				Some((inner, EqKind::OptionInner))
			} else {
				Some((tag_inner, EqKind::SelfEq))
			}
		}
		Value::Bool(_) => Some((tag_inner, EqKind::Bool)),
		Value::String(_) => None,
		Value::UnionDescriminator(id) => {
			if let TypeKind::Union { matches, .. } = kind {
				// only a value-less arm can be compared by constructing the variant
				let void = matches.iter().any(|(_, n, vt)| {
					sanitize(n.as_str()) == sanitize(id.as_str()) && input.is_void(vt.target)
				});
				void.then_some((tag_inner, EqKind::SelfEq))
			} else {
				None
			}
		}
	}
}

/// Types embedded *by value* in `id`'s generated representation (no heap
/// indirection). Used to decide which union arms must be boxed to break a cycle.
/// `array` (Vec), `option` (Option<Box>), `buffer`/`pstring` (leaves) do not embed
/// their element by value, so their specialization types are not edges here.
fn value_edges(types: &TypeMap, id: TypeId) -> Vec<TypeId> {
	match &types[id].kind {
		TypeKind::Container { fields, .. } => fields
			.iter()
			.filter(|f| f.load_from.is_none())
			.map(|f| f.ty.target)
			.collect(),
		TypeKind::Union {
			matches, default, ..
		} => matches
			.iter()
			.map(|(_, _, t)| t.target)
			.chain(default.iter().map(|(_, t)| t.target))
			.collect(),
		TypeKind::Alias(t) => vec![t.target],
		// bitfield/bitflags/registryEntryHolder store their `fields` by value; native
		// generics live behind Vec/Box/leaves and are not by-value edges.
		TypeKind::Native(n) => n.fields.iter().map(|(_, t)| t.target).collect(),
		TypeKind::Void => vec![],
	}
}

/// Whether `target` is reachable from `from` purely via by-value containment. A union
/// arm with payload `from` belonging to union `target` must be boxed exactly when this
/// holds (the arm closes a value cycle), and need not be boxed otherwise.
fn value_reaches(types: &TypeMap, from: TypeId, target: TypeId) -> bool {
	fn go(
		types: &TypeMap,
		cur: TypeId,
		target: TypeId,
		seen: &mut std::collections::HashSet<TypeId>,
	) -> bool {
		if cur == target {
			return true;
		}
		if !seen.insert(cur) {
			return false;
		}
		value_edges(types, cur)
			.into_iter()
			.any(|n| go(types, n, target, seen))
	}
	go(types, from, target, &mut std::collections::HashSet::new())
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
		FieldOrConst::Const(_) => return quote!(Into::<$target>::into(todo!("const"))),
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
				quote!(let _0: &$(types.ty(field.to.get())) = &$(types.virt(field.from.get(), name.clone()))(_0);)
			}
		})
	}
	if let Some(target) = target {
		indexing.push(quote!(Into::<$target>::into(*_0)));
	} else {
		indexing.push(quote!(_0))
	}
	quote!({$(for i in indexing => $i)})
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
	let meta = RustMeta::new(input, reg);

	// Unions referenced as another union's discriminant must expose `virtual_tag`
	// even when they are external-tagged (the referencing union calls it to compute
	// the discriminant). Inline-tag unions always expose it (for their own serialize).
	let mut unions_as_tag: std::collections::HashSet<TypeId> = std::collections::HashSet::new();
	for def in input.0.values() {
		if let TypeKind::Union { tag_type, .. } = &def.kind {
			let mut inner = *tag_type;
			while let TypeKind::Container {
				fields,
				delegate_all: Some(d),
				..
			} = &input[inner].kind
			{
				inner = fields
					.iter()
					.find(|f| &f.name == d)
					.expect("delegate target exists")
					.ty
					.target;
			}
			if matches!(input[inner].kind, TypeKind::Union { .. }) {
				unions_as_tag.insert(inner);
			}
		}
	}

	// For every union arm, record which `is_equal_{b}` helpers its tag type must
	// expose. Each tag-capable module emits these from its own entry; the union
	// codegen calls them instead of inlining tag-kind-specific comparisons.
	let mut eq_impls: BTreeMap<TypeId, std::collections::BTreeSet<(TypeId, EqKind)>> =
		BTreeMap::new();
	for def in input.0.values() {
			if let TypeKind::Union {
			tag_type, matches, ..
		} = &def.kind
		{
			let inner = resolve_tag_inner(input, *tag_type);
			for (value, _, _) in matches {
				if let Some(t) = eq_target(input, inner, value) {
					eq_impls.entry(inner).or_default().insert(t);
				}
			}
		}
	}

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
		let assert_tag_call = |field: &mir::ContainerField| -> Tokens {
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
				if matches!(
					f.0.first().map(|i| &i.segment),
					Some(FieldRefSegment::Arg(_))
				) {
					return quote!();
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
						pub struct Ty {
							$(for field in fields.iter() join($['\r']) =>
								$(if field.load_from.is_none() {
									pub $(rname(&field.name)): $(meta.ty(field.ty.target)),
								} else {
									$("// ") $(&field.name) $("- virtual field")
								})
							)
						}
						pub fn serialize(_0: &Ty, output: &mut $poutput) -> $presult<()> {
							$(for field in fields.iter() join($['\r']) =>
								$(assert_tag_call(field))
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
									$(rname(&field.name)): $(format!("f_{}", field.name.as_str()))
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
					let tag_is_union = matches!(input[tag_inner].kind, TypeKind::Union { .. });
					let tag_is_option = matches!(
						&input[tag_inner].kind,
						TypeKind::Native(mir::NativeType {
							specializer: Some((s, _)),
							..
						}) if s.as_str() == "option"
					);
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
						match value {
							Value::Signed(n) => quote!($(n.to_string())),
							Value::Unsigned(n) => quote!($(n.to_string())),
							Value::Bool(b) => quote!($(b.to_string())),
							Value::String(s) => quote!($(quoted(s.as_str())).into()),
							Value::UnionDescriminator(_) => {
								quote!(todo!("constructing a non-scalar tag"))
							}
						}
					};
					// Build per-arm tokens. A void-typed arm (the deduplicated canonical
					// void) becomes a value-less enum variant; an inline union's default
					// arm stores the read tag value so it can be written back.
					let void_of = |t: TypeId| input.is_void(t);
					// Box an arm payload only when it can reach this union by value
					// (closes a cycle); otherwise hold it inline.
					let boxed_of = |t: TypeId| value_reaches(input, t, *id);
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
							let boxed = boxed_of(vty.target);
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
						let boxed = boxed_of(dty.target);
						let dpay = de_value(dty.target, dty, boxed);
						match (is_inline, void) {
							(true, true) => {
								variants_decl.push(quote!($(&d)($(meta.ty(tag_ty))),));
								ser_arms.push(quote!(Ty::$(&d)(_) => Ok(()),));
								size_arms.push(quote!(Ty::$(&d)(_) => 0,));
								vtag_arms.push(quote!(Ty::$(&d)(__t) => __t.clone(),));
								quote!(Ok(Ty::$(&d)(__tagv)))
							}
							(true, false) => {
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
								quote!(Ok(Ty::$(&d)(__tagv, $dpay)))
							}
							(false, true) => {
								variants_decl.push(quote!($(&d),));
								ser_arms.push(quote!(Ty::$(&d) => Ok(()),));
								size_arms.push(quote!(Ty::$(&d) => 0,));
								vtag_arms.push(
									quote!(Ty::$(&d) => todo!("tag of the external default arm"),),
								);
								assert_arms.push(quote!(Ty::$(&d) => true,));
								quote!(Ok(Ty::$(&d)))
							}
							(false, false) => {
								variants_decl
									.push(quote!($(&d)($(payload_ty(dty.target, boxed))),));
								ser_arms.push(
									quote!(Ty::$(&d)(__v) => $(meta.serialize(dty.target))(__v, output),),
								);
								size_arms.push(
									quote!(Ty::$(&d)(__v) => $(meta.size_of(dty.target))(__v),),
								);
								vtag_arms.push(
									quote!(Ty::$(&d)(_) => todo!("tag of the external default arm"),),
								);
								assert_arms.push(quote!(Ty::$(&d)(_) => true,));
								quote!(Ok(Ty::$(&d)($dpay)))
							}
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
					let derive_eq = my_eq.iter().any(|(_, k)| *k == EqKind::SelfEq);
					let all_void = matches.iter().all(|(_, _, vt)| void_of(vt.target))
						&& default.as_ref().is_none_or(|(_, dt)| void_of(dt.target));
					let derive_attr = if derive_eq {
						if all_void {
							quote!(#[derive(PartialEq, Eq)])
						} else {
							quote!(#[derive(PartialEq)])
						}
					} else {
						quote!()
					};
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
						$(if is_inline || unions_as_tag.contains(id) =>
							pub fn virtual_tag(_0: &Ty) -> $(meta.ty(tag_ty)) {
								$(if is_empty => match *_0 {})
								$(if !is_empty {
									$(if tag_is_union || tag_is_option {
										todo!("reconstructing a non-scalar tag")
									} else {
										$match_head {
											$(for a in &vtag_arms join($['\r']) => $a)
										}
									})
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
					let Some(codegen) = reg.codegen::<Tokens>(specializer.as_str()) else {
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
							variant,
							native,
							params: &ty.params,
							rust: &meta,
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
