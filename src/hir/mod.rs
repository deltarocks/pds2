use crate::ast::{Ident, Path, Reference, Value};
use anyhow::Result;
use std::{cell::Cell, collections::BTreeMap};
mod from_ast;
pub use from_ast::parse_ast;
mod desugar;
pub use to_ast::to_ast;
mod id_generator;
mod to_ast;
pub use id_generator::{IdGenerator, TypeId};

#[derive(Debug)]
pub enum NamespaceItem {
    Inner(Namespace),
    Type(Type),
}

#[derive(Debug)]
pub struct Namespace {
    pub items: BTreeMap<Ident, NamespaceItem>,
}

#[derive(Debug)]
pub enum ContainerFieldType {
    Normal(TypeKind),
    Virtual(Reference, TypeKind),
}

#[derive(Debug)]
pub struct Container {
    pub virt: bool,
    pub fields: Vec<(Ident, ContainerFieldType)>,
}

#[derive(Debug)]
pub struct Union {
    tag: Reference,
    matches: Vec<(Value, Ident, TypeKind)>,
    default: Option<(Ident, Box<TypeKind>)>,
}

#[derive(Debug)]
pub struct Array {
    pub length: Result<Reference, i64>,
    pub ty: Box<TypeKind>,
}

#[derive(Debug, Clone)]
pub enum NativeTypeKind {
    Int(u32),
    Uint(u32),
    Opaque,
}

#[derive(Debug)]
pub enum TypeKind {
    Container(Container),
    Union(Union),
    Array(Array),
    Reference(Result<TypeId, Path>, Vec<(Ident, Reference)>),
    Native(NativeTypeKind),
}
impl TypeKind {
    fn assert_reference(&self) {
        match self {
            TypeKind::Reference(_, _) => {}
            n => unreachable!("should not have nested: {:?}", n),
        }
    }
}

pub struct Type {
    _id: Cell<TypeId>,
    pub params: Vec<Ident>,
    pub kind: TypeKind,
}

impl std::fmt::Debug for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Type")
            .field("id", &self._id.get())
            .field("params", &self.params)
            .field("kind", &self.kind)
            .finish()
    }
}
impl Type {
    pub fn id(&self) -> TypeId {
        let id = self._id.get();
        assert!(id.is_valid());
        id
    }
    pub fn assign_id(&self, id: &IdGenerator) {
        if !self._id.get().is_valid() {
            self._id.set(id.next())
        }
    }
    pub fn is_native(&self) -> bool {
        matches!(self.kind, TypeKind::Native(_))
    }
}
