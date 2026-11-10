use crate::{
	ast::{FieldRef, Ident, Path},
	hir::{self, Reference, ReferenceKind, TypeId},
	mir::{self, FieldItem},
	specializer::Registry,
};
use anyhow::Result;
use std::collections::HashMap;

use super::{Definition, Expr, Field, NativeType, Type, TypeKind, TypeMap, Value};

impl Type {
	fn from_hir(kind: &hir::TypeKind) -> Self {
		match kind {
			hir::TypeKind::Reference(Reference {
				ty: ReferenceKind::Resolved(target),
				args,
				types,
				..
			}) => {
				assert!(types.is_empty(), "monomorphization should be done");
				Self {
					target: *target,
					args: args
						.iter()
						.map(|(n, r)| (n.clone(), Expr::from_hir(r)))
						.collect(),
				}
			}
			t => unreachable!(
				"only resolved references should be left after HIR desugaring: {:?}",
				t
			),
		}
	}
}

impl Value {
	fn from_hir(hir: &hir::Value) -> Self {
		match hir {
			hir::Value::Number(n) => Self::Signed(*n),
			hir::Value::Float(f) => Self::Float(f.0),
			hir::Value::Bytes(b) => Self::Bytes(b.clone()),
			hir::Value::Bool(b) => Self::Bool(*b),
			hir::Value::String(s) => Self::String(s.clone()),
			hir::Value::UnionDescriminator(ident) => Self::UnionDescriminator(ident.clone()),
		}
	}
}

impl TypeKind {
	fn from_hir(hir: &hir::TypeKind) -> Result<Self> {
		Ok(match hir {
			// An exported alias def surviving replace_aliases: keep it as a named
			// forwarding type.
			hir::TypeKind::Reference(Reference {
				ty: ReferenceKind::Resolved(_),
				..
			}) => Self::Alias(Type::from_hir(hir)),
			hir::TypeKind::Reference(r) => unreachable!("unresolved reference in MIR: {r:?}"),
			hir::TypeKind::Native(native) => Self::Native(NativeType {
				virtual_fields: native
					.virtual_fields
					.iter()
					.map(|(name, ty)| (name.clone(), Type::from_hir(ty)))
					.collect(),
				fields: native
					.fields
					.iter()
					.map(|(name, ty)| (name.clone(), Type::from_hir(ty)))
					.collect(),
				specializer: native
					.specializer
					.clone()
					.map(|s| (s, native.variant.clone())),
				body: native
					.body
					.iter()
					.map(|b| mir::BodyItem {
						kind: b.kind.clone(),
						name: b.name.clone(),
						args: b
							.args
							.iter()
							.map(|(n, a)| (n.clone(), Expr::from_hir(a)))
							.collect(),
					})
					.collect(),
				const_args: native.const_args.clone(),
				ref_args: native
					.ref_args
					.iter()
					.map(|(n, r)| (n.clone(), Expr::from_hir(r)))
					.collect(),
				arg_types: native
					.arg_types
					.iter()
					.map(|(n, t)| (n.clone(), Type::from_hir(t).target))
					.collect(),
				arg_virtuals: native.arg_virtuals.clone(),
				is_void: native.is_void,
				scalar: native.scalar.clone(),
				attrs: native.attrs.clone(),
				specialization: native
					.specialization
					.iter()
					.map(|(n, v)| (n.clone(), Type::from_hir(v)))
					.collect(),
			}),
		})
	}
}

impl Definition {
	fn from_hir(name: Path, hir: &hir::Type) -> Result<Self> {
		assert!(hir.id().is_valid());

		let param_types: Vec<(Ident, TypeId)> = hir
			.param_types
			.iter()
			.map(|(n, t)| (n.clone(), Type::from_hir(t).target))
			.collect();
		Ok(Definition {
			name,
			params: hir
				.params
				.iter()
				.map(|p| {
					let declared = param_types.iter().find(|(n, _)| n == p).map(|(_, t)| *t);
					(p.clone(), declared.unwrap_or_default())
				})
				.collect(),
			param_types,
			kind: TypeKind::from_hir(&hir.kind)?,
		})
	}
}

impl Field {
	fn from_hir(hir: &FieldRef) -> Self {
		assert!(hir.up == 0, "fields should be absolute, got {hir:?}");

		Self(
			hir.path
				.0
				.iter()
				.cloned()
				.map(|segment| FieldItem {
					from: Default::default(),
					segment,
					to: Default::default(),
				})
				.collect(),
		)
	}
}

impl Expr {
	fn from_hir(hir: &hir::Expr) -> Self {
		match hir {
			hir::Expr::Field(field_ref) => Expr::Field(Field::from_hir(field_ref)),
			hir::Expr::Const(value) => Expr::Const(Value::from_hir(value)),
			hir::Expr::Binary(op, a, b) => {
				Expr::Binary(*op, Box::new(Expr::from_hir(a)), Box::new(Expr::from_hir(b)))
			}
			hir::Expr::Record(fields) => Expr::Record(
				fields
					.iter()
					.map(|(n, e)| (n.clone(), Expr::from_hir(e)))
					.collect(),
			),
			hir::Expr::List(items) => Expr::List(items.iter().map(Expr::from_hir).collect()),
			hir::Expr::Serialized(e) => Expr::Serialized(Box::new(Expr::from_hir(e))),
			hir::Expr::Cast(e, t) => {
				Expr::Cast(Box::new(Expr::from_hir(e)), Type::from_hir(t).target)
			}
		}
	}
}

pub fn from_hir_internal(
	hir: &hir::Namespace,
	path: &mut Vec<Ident>,
	out: &mut HashMap<TypeId, Definition>,
) -> Result<()> {
	for (name, item) in hir.items.iter() {
		path.push(name.clone());
		match item {
			hir::NamespaceItem::Inner(ns) => from_hir_internal(ns, path, out)?,
			hir::NamespaceItem::Type(d) => {
				// Template `def_native`s are compile-time only: their use-sites are
				// inlined/monomorphized away, so they are not emitted to MIR.
				if matches!(&d.kind, hir::TypeKind::Native(n) if n.is_template()) {
					path.pop().expect("stack underflow");
					continue;
				}
				let old = out.insert(d.id(), Definition::from_hir(Path::new(path.clone()), d)?);
				assert!(old.is_none());
			}
		}
		path.pop().expect("stack underflow");
	}
	Ok(())
}

pub fn from_hir(hir: &hir::Namespace, reg: &Registry) -> Result<TypeMap> {
	let mut out = HashMap::new();
	from_hir_internal(hir, &mut Vec::new(), &mut out)?;
	let mut out = TypeMap(out.into_iter().collect());
	super::pass::resolve_types(&mut out)?;
	super::pass::elaborate(&mut out, reg)?;
	Ok(out)
}
