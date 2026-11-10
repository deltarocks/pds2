use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::ast::{FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Field, FieldOrConst, TypeKind, TypeMap, Value};
use crate::specializer::{container, union, Scalar};

/// Sanitize an arbitrary string (`minecraft:foo`) into an identifier, mirroring the
/// upgrade-side variant naming.
pub fn sanitize(key: &str) -> String {
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
pub enum EqKind {
	/// `a == b` -- tag and arm value share the type (requires `PartialEq`); used by
	/// scalar leaves (int/bool), strings, and value-less tag enums.
	SelfEq,
	/// `&virtual_tag(a) == b` -- a union tag matched by its wire scalar.
	VirtualTag,
	/// `a.as_deref() == Some(b)` -- an `option` tag matched by its inner value.
	OptionInner,
	/// `a == b` against a bool literal; emitted as `is_equal_bool(a: &Ty, b: &bool)`,
	/// named by value kind rather than tag type id so the codegen carries no
	/// bool-special-case lookup.
	Bool,
}

pub fn local(name: &Ident) -> String {
	format!("f_{}", name.as_str())
}

pub fn eq_fn_name(other: TypeId, kind: EqKind) -> String {
	match kind {
		EqKind::Bool => "is_equal_bool".to_owned(),
		_ => format!("is_equal_{}", other.raw()),
	}
}

/// The inner value type of an `option` native, if `id` is one.
pub fn option_inner(input: &TypeMap, id: TypeId) -> Option<TypeId> {
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

/// The `(b type id, comparison kind)` an arm `value` needs from `tag_inner`'s
/// module, or `None` when the value can never match this tag type (the arm is
/// dead) -- the union codegen falls back to a discriminant check or a literal
/// `false` in that case.
pub fn eq_target(input: &TypeMap, tag_inner: TypeId, value: &Value) -> Option<(TypeId, EqKind)> {
	let union = union::view_of(input, tag_inner);
	match value {
		Value::Signed(_) | Value::Unsigned(_) => {
			if let Some(u) = &union {
				Some((u.tag_type, EqKind::VirtualTag))
			} else if let Some(inner) = option_inner(input, tag_inner) {
				Some((inner, EqKind::OptionInner))
			} else {
				Some((tag_inner, EqKind::SelfEq))
			}
		}
		Value::Bool(_) => Some((tag_inner, EqKind::Bool)),
		Value::String(_) => None,
		Value::UnionDescriminator(id) => {
			let u = union?;
			// only a value-less arm can be compared by constructing the variant
			let void = u.arms.iter().any(|(_, n, vt)| {
				sanitize(n.as_str()) == sanitize(id.as_str()) && input.is_void(vt.target)
			});
			void.then_some((tag_inner, EqKind::SelfEq))
		}
	}
}

pub fn unions_as_tag(input: &TypeMap) -> HashSet<TypeId> {
	let mut out = HashSet::new();
	for id in input.0.keys() {
		if let Some(u) = union::view_of(input, *id) {
			if union::view_of(input, u.tag_type).is_some() {
				out.insert(u.tag_type);
			}
		}
	}
	out
}

pub fn eq_impls(input: &TypeMap) -> BTreeMap<TypeId, BTreeSet<(TypeId, EqKind)>> {
	let mut out: BTreeMap<TypeId, BTreeSet<(TypeId, EqKind)>> = BTreeMap::new();
	for id in input.0.keys() {
		if let Some(u) = union::view_of(input, *id) {
			for (value, _, _) in &u.arms {
				if let Some(t) = eq_target(input, u.tag_type, value) {
					out.entry(u.tag_type).or_default().insert(t);
				}
			}
		}
	}
	out
}

pub enum TagRepr {
	Literal,
	OptionSome(TypeId),
	Variant(TypeId, Ident),
}

fn value_eq(a: &Value, b: &Value) -> bool {
	let int = |v: &Value| match v {
		Value::Signed(n) => Some(*n),
		Value::Unsigned(n) => i128::try_from(*n).ok(),
		_ => None,
	};
	match (a, b) {
		(Value::Bool(a), Value::Bool(b)) => a == b,
		(Value::String(a), Value::String(b)) => a == b,
		(Value::UnionDescriminator(a), Value::UnionDescriminator(b)) => {
			sanitize(a.as_str()) == sanitize(b.as_str())
		}
		_ => matches!((int(a), int(b)), (Some(a), Some(b)) if a == b),
	}
}

pub fn tag_repr(input: &TypeMap, tag_ty: TypeId, value: &Value) -> Option<TagRepr> {
	if let Some(u) = union::view_of(input, tag_ty) {
		return u
			.arms
			.iter()
			.find(|(v, n, ty)| {
				input.is_void(ty.target)
					&& match value {
						Value::UnionDescriminator(id) => {
							sanitize(n.as_str()) == sanitize(id.as_str())
						}
						other => value_eq(v, other),
					}
			})
			.map(|(_, n, _)| TagRepr::Variant(tag_ty, (*n).clone()));
	}
	if container::is_container(input, tag_ty) {
		return None;
	}
	match &input[tag_ty].kind {
		TypeKind::Alias(t) => tag_repr(input, t.target, value),
		_ if option_inner(input, tag_ty).is_some() => match value {
			Value::UnionDescriminator(_) => None,
			_ => Some(TagRepr::OptionSome(tag_ty)),
		},
		_ => match value {
			Value::UnionDescriminator(_) => None,
			_ => Some(TagRepr::Literal),
		},
	}
}

pub fn virtuals_used(input: &TypeMap) -> HashSet<(TypeId, Ident)> {
	let mut out = HashSet::new();
	let mut scan = |f: &Field| {
		for item in &f.0 {
			if let FieldRefSegment::Virtual(n) = &item.segment {
				out.insert((item.from.get(), n.clone()));
			}
		}
	};
	for def in input.0.values() {
		let mut uses: Vec<&mir::Type> = Vec::new();
		match &def.kind {
			TypeKind::Native(n) => {
				uses.extend(n.specialization.iter().map(|(_, t)| t));
				uses.extend(n.fields.iter().map(|(_, t)| t));
				uses.extend(n.virtual_fields.iter().map(|(_, t)| t));
				for (_, f) in &n.ref_args {
					scan(f);
				}
				for item in &n.body {
					for (_, a) in &item.args {
						if let FieldOrConst::Field(f) = a {
							scan(f);
						}
					}
				}
			}
			TypeKind::Alias(t) => uses.push(t),
			TypeKind::Void => {}
		}
		for u in uses {
			for (_, arg) in &u.args {
				if let FieldOrConst::Field(f) = arg {
					scan(f);
				}
			}
		}
	}
	out
}

pub fn scalar_of(input: &TypeMap, id: TypeId) -> Option<Scalar> {
	match &input[id].kind {
		TypeKind::Native(n) => n.scalar.as_ref().and_then(|s| Scalar::parse(s.as_str())),
		TypeKind::Alias(t) => scalar_of(input, t.target),
		_ => None,
	}
}
