use crate::{
	ast::{self, FieldRef, Ident, Path},
	hir::{self, FieldRefOrConst, Reference, ReferenceKind, TypeId},
	mir::{self, FieldItem},
	should_be_desugared,
};
use anyhow::{bail, Result};
use std::collections::HashMap;

use super::{Definition, Field, FieldOrConst, NativeType, Type, TypeKind, TypeMap, Value};

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
						.map(|(n, r)| (n.clone(), FieldOrConst::from_hir(r)))
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
			hir::Value::String(_) => todo!(),
			hir::Value::UnionDescriminator(ident) => todo!(),
		}
	}
}

impl TypeKind {
	fn from_hir(hir: &hir::TypeKind) -> Result<Self> {
		Ok(match hir {
			hir::TypeKind::Container(c) => Self::Container {
				virt: c.virt,
				fields: c
					.fields
					.iter()
					.map(|(name, f)| {
						Ok(match f {
							hir::ContainerFieldType::Normal(t) => mir::ContainerField {
								name: name.clone(),
								ty: Type::from_hir(t),
								load_from: None,
							},
							hir::ContainerFieldType::Virtual(r, t) => mir::ContainerField {
								name: name.clone(),
								ty: Type::from_hir(t),
								load_from: Some(Field::from_hir(r)),
							},
							hir::ContainerFieldType::Delegate(_) => should_be_desugared!(f),
						})
					})
					.collect::<Result<_>>()?,
				delegate_all: c.delegate_all.clone(),
			},
			hir::TypeKind::Union(u) => {
				Self::Union {
					tag: Field::from_hir(&u.tag),
					tag_type: TypeId::default(),
					matches: u
						.values
						.iter()
						.map(|(value, map, ty)| {
							(
								match value {
									hir::Value::Number(n) => Value::Signed(*n),
									hir::Value::String(n) => Value::String(n.clone()),
									hir::Value::UnionDescriminator(id) => {
										Value::UnionDescriminator(id.clone())
									}
								},
								map.clone(),
								Type::from_hir(ty),
							)
						})
						.collect(),
					// TODO
					default: None,
				}
			}
			hir::TypeKind::Reference(Reference { .. }) => todo!("type alias: {hir:?}"),
			hir::TypeKind::Native(hir::NativeType {
				virtual_fields,
				fields,
				specializer,
				specialization,
				..
			}) => Self::Native(NativeType {
				virtual_fields: virtual_fields
					.iter()
					.map(|(name, ty)| (name.clone(), Type::from_hir(ty)))
					.collect(),
				fields: fields
					.iter()
					.map(|(name, ty)| (name.clone(), Type::from_hir(ty)))
					.collect(),
				specializer: specializer.clone(),
				specialization: specialization
					.iter()
					.map(|(n, v)| (n.clone(), Type::from_hir(v)))
					.collect(),
			}),
			hir::TypeKind::Void => Self::Void,
		})
	}
}

impl Definition {
	fn from_hir(name: Path, hir: &hir::Type) -> Result<Self> {
		assert!(hir.id().is_valid());

		Ok(Definition {
			name,
			params: hir
				.params
				.iter()
				.map(|p| (p.clone(), TypeId::default()))
				.collect(),
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

impl FieldOrConst {
	fn from_hir(hir: &FieldRefOrConst) -> Self {
		match hir {
			FieldRefOrConst::FieldRef(field_ref) => FieldOrConst::Field(Field::from_hir(field_ref)),
			FieldRefOrConst::Const(value) => FieldOrConst::Const(Value::from_hir(value)),
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
				let old = out.insert(d.id(), Definition::from_hir(Path::new(path.clone()), d)?);
				assert!(old.is_none());
			}
		}
		path.pop().expect("stack underflow");
	}
	Ok(())
}

pub fn from_hir(hir: &hir::Namespace) -> Result<TypeMap> {
	let mut out = HashMap::new();
	from_hir_internal(hir, &mut Vec::new(), &mut out)?;
	let mut out = TypeMap(out.into_iter().collect());
	super::pass::resolve_types(&mut out)?;
	super::pass::remove_delegation(&mut out)?;
	Ok(out)
}
