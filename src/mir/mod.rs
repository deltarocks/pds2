mod from_hir;
use std::{cell::Cell, collections::HashMap, ops::Index};

pub use from_hir::from_hir;
mod pass;

use crate::{
    ast::{BaseReference, Ident, Path},
    hir::{NativeTypeKind, TypeId},
};

#[derive(Debug)]
pub struct Type {
    pub target: TypeId,
    pub args: Vec<(Ident, Field)>,
}

#[derive(Debug)]
pub struct Definition {
    pub name: Path,
    pub params: Vec<(Ident, TypeId)>,
    pub kind: TypeKind,
}

#[derive(Debug)]
pub struct ContainerField {
    pub name: Ident,
    pub ty: Type,
    pub load_from: Option<Field>,
}

#[derive(Debug)]
pub enum TypeKind {
    Container {
        virt: bool,
        fields: Vec<ContainerField>,
    },
    Union {
        matches: Vec<(Value, Ident, Type)>,
        default: (Ident, Type),
    },
    FixedSizeArray {
        size: usize,
        t: Type,
    },
    DynamicArray {
        size_ref: BaseReference,
        t: Type,
    },
    Native(NativeTypeKind),
}

impl TypeKind {
    pub fn is_native(&self) -> bool {
        matches!(self, TypeKind::Native(_))
    }
}

#[derive(Debug)]
pub enum Value {
    Unsigned(u128),
    Signed(i128),
    String(String),
}

pub struct TypeMap(pub HashMap<TypeId, Definition>);
impl Index<TypeId> for TypeMap {
    type Output = Definition;

    fn index(&self, index: TypeId) -> &Self::Output {
        &self.0[&index]
    }
}

#[derive(Debug, Clone)]
pub enum FieldItemKind {
    /// Can only be first
    Argument,
    Field,
    Virtual,
}

#[derive(Debug, Clone)]
pub struct FieldItem {
    pub from: Cell<TypeId>,
    pub kind: FieldItemKind,
    pub name: Ident,
    pub to: Cell<TypeId>,
}

#[derive(Debug, Clone)]
pub struct Field(pub Vec<FieldItem>);
