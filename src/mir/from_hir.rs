use crate::{
	ast::{self, Ident, Path, Reference},
	hir::{self, TypeId},
	mir::{self, FieldItem},
	should_be_desugared,
};
use anyhow::{bail, Result};
use std::{collections::HashMap, rc::Rc};

use super::{Definition, Field, NativeTypeKind, Type, TypeKind, TypeMap, Value};

impl Type {
	fn from_hir(kind: &hir::TypeKind) -> Self {
		match kind {
			hir::TypeKind::Reference {
				ty: Ok(target),
				args,
				types,
			} => {
				assert!(types.is_empty(), "monomorphization should be done");
				Self {
					target: *target,
					args: args
						.iter()
						.map(|(n, r)| (n.clone(), Field::from_hir(r)))
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
	fn from_hir(hir: &ast::Value) -> Result<Self> {
		Ok(match hir {
			ast::Value::String { string, .. } => Self::String(string.clone()),
			ast::Value::Int(i) => Self::Signed(*i as i128),
			_ => bail!("unexpected value type"),
		})
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
			hir::TypeKind::Union(u) => Self::Union {
				tag: match &u.tag {
					hir::UnionTag::LoadFrom(r) => Field::from_hir(r),
					hir::UnionTag::Type(_) => should_be_desugared!(u.tag),
				},
				matches: u
					.matches
					.iter()
					.map(|v| {
						(
							mir::Value::from_hir(&v.0).unwrap(),
							v.1.clone(),
							Type::from_hir(&v.2),
						)
					})
					.collect(),
				default: u
					.default
					.as_ref()
					.map(|d| (d.0.clone(), Type::from_hir(&d.1))),
			},
			hir::TypeKind::Array(a) => match &a.length {
				hir::ArrayLength::LoadFrom(r) => Self::DynamicArray {
					size_ref: Field::from_hir(r),
					t: Type::from_hir(&a.ty),
				},
				hir::ArrayLength::Fixed(v) => Self::FixedSizeArray {
					size: *v,
					t: Type::from_hir(&a.ty),
				},
				hir::ArrayLength::Type(_) => should_be_desugared!(a),
			},
			hir::TypeKind::Reference { .. } => todo!("type alias"),
			hir::TypeKind::Native(hir::NativeTypeKind::Int(size)) => {
				Self::Native(NativeTypeKind::Int(*size))
			}
			hir::TypeKind::Native(hir::NativeTypeKind::Uint(size)) => {
				Self::Native(NativeTypeKind::Uint(*size))
			}
			hir::TypeKind::Native(hir::NativeTypeKind::Opaque {
				virtual_fields,
				specializer,
				specialization,
			}) => Self::Native(NativeTypeKind::Opaque {
				virtual_fields: virtual_fields
					.iter()
					.map(|(name, ty)| (name.clone(), Type::from_hir(ty)))
					.collect(),
				specializer: specializer.clone(),
				specialization: specialization
					.iter()
					.map(|(n, v)| (n.clone(), Type::from_hir(v)))
					.collect(),
			}),
			hir::TypeKind::Native(hir::NativeTypeKind::Pending) => unreachable!(),
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
	fn from_hir(hir: &Reference) -> Self {
		assert!(hir.up == 0);

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
