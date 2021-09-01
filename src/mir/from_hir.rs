use crate::{
    ast::{self, Ident, Path, Reference},
    hir::{self, TypeId},
    mir::{self, FieldItem, FieldItemKind},
};
use anyhow::{anyhow, bail, Result};
use std::{collections::HashMap, convert::TryInto};

use super::{Definition, Field, Type, TypeKind, TypeMap, Value};

impl Type {
    fn from_hir(kind: &hir::TypeKind) -> Self {
        match kind {
            hir::TypeKind::Reference(Ok(target), args) => Self {
                target: *target,
                args: args
                    .iter()
                    .map(|(n, r)| (n.clone(), Field::from_hir(r)))
                    .collect(),
            },
            _ => unreachable!(),
        }
    }
}

impl Value {
    fn from_hir(hir: &ast::Value) -> Result<Self> {
        Ok(match hir {
            ast::Value::String { string, is_block } => Self::String(string.clone()),
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
                        })
                    })
                    .collect::<Result<_>>()?,
            },
            hir::TypeKind::Union(u) => Self::Union {
                tag: u.tag.clone().try_into()?,
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
                Ok(r) => Self::DynamicArray {
                    size_ref: r.clone().try_into().unwrap(),
                    t: Type::from_hir(&a.ty),
                },
                Err(s) => Self::FixedSizeArray {
                    size: (*s).try_into().map_err(|_| anyhow!("invalid size"))?,
                    t: Type::from_hir(&a.ty),
                },
            },
            hir::TypeKind::Reference(_, _) => todo!(),
            hir::TypeKind::Native(n) => Self::Native(n.clone()),
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
                .enumerate()
                .map(|(i, (virt, name))| FieldItem {
                    from: Default::default(),
                    kind: match () {
                        _ if *virt && i == 0 => FieldItemKind::Argument,
                        _ if *virt => FieldItemKind::Virtual,
                        _ => FieldItemKind::Field,
                    },
                    name: name.clone(),
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
    super::pass::resolve_types(&mut out)?;
    let mut out = TypeMap(out.into_iter().collect());
    super::pass::resolve_field_types(&mut out);
    Ok(out)
}
