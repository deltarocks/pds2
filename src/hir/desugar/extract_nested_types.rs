//! Extract nested types desugaring pass
//!
//! This pass removes expression nesting, moving nested expressions to namespace root,
//! and replacing usage with type reference
//!
//! ```pds
//! def(struct_a) => container {
//!     field(field_b) => container {
//!         field(field_c) => u8;
//!     };
//! };
//!
//! =>
//!
//! def(struct_a) => container {
//!     field(field_b) => root::struct_a_field_b;
//! };
//! def(struct_a_field_b) => container {
//!     field(field_c) => u8;
//! };
//! ```
//!
//! Because types can refer to parent struct fields - this pass also rewrites references to arguments
//! ```pds
//! def(struct_a) => container {
//!     field(size) => usize;
//!     field(data) => array(length: ../size);
//! };
//!
//! =>
//!
//! def(struct_a) => container {
//!     field(size) => usize;
//!     field(data) => struct_a_data(size: ./size);
//! };
//! def(struct_a_data) => array(length: ./@size)
//! ```
//!
//! After this pass types can only have other references as thier children, and there is no references to parent structs

use std::cell::Cell;

use crate::{
    ast::{BaseReference, Ident, Reference},
    hir::{IdGenerator, Namespace, NamespaceItem, NativeTypeKind, Type, TypeKind},
};

impl Namespace {
    fn extract_nested_types(&mut self, id: IdGenerator) {
        let mut receiver = TypeReceiver {
            id: id.clone(),
            path: vec![],
            extracted: vec![],
        };

        for (n, item) in self.items.iter_mut() {
            match item {
                crate::hir::NamespaceItem::Inner(ns) => ns.extract_nested_types(id.clone()),
                crate::hir::NamespaceItem::Type(t) => {
                    receiver.push(n.clone());
                    t.extract_nested_types(&mut receiver);
                    receiver.pop();
                }
            }
        }
        for (path, ty) in receiver.extracted {
            let mut name = path
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join("_");
            while self.items.contains_key(&Ident::new(&name)) {
                name += "_yet_another";
            }
            self.items
                .insert((&name as &str).into(), NamespaceItem::Type(ty));
        }
    }
}

struct TypeReceiver {
    id: IdGenerator,
    path: Vec<Ident>,
    extracted: Vec<(Vec<Ident>, Type)>,
}

impl TypeReceiver {
    fn push(&mut self, ident: impl Into<Ident>) {
        self.path.push(ident.into());
    }
    fn pop(&mut self) {
        self.path.pop().expect("stack underflow");
    }

    fn extract(&mut self, ty: &mut TypeKind) {
        ty.extract_nested_types(self);

        let mut collector = ReferenceCollector::default();
        ty.rewrite_references(&mut collector);

        match ty {
            TypeKind::Reference(_, _) => return,
            _ => {}
        }
        let vty = std::mem::replace(ty, TypeKind::Native(NativeTypeKind::Opaque));
        let id = self.id.next();
        self.extracted.push((
            self.path.clone(),
            Type {
                _id: Cell::new(id),
                params: collector.params,
                kind: vty,
            },
        ));
        *ty = TypeKind::Reference(Ok(id), collector.args)
    }
}

#[derive(Default)]
struct ReferenceCollector {
    args: Vec<(Ident, Reference)>,
    params: Vec<Ident>,
}
impl ReferenceCollector {
    fn collect(&mut self, r: &mut Reference) {
        if r.up == 0 {
            return;
        }
        let mut orig = r.clone();
        orig.up -= 1;
        let name: Ident = r.name().as_str().into();

        self.args.push((name.clone(), orig));

        *r = Reference {
            up: 0,
            path: BaseReference(vec![(true, name.clone())]),
        };
        self.params.push(name);
    }
}

impl TypeKind {
    fn rewrite_references(&mut self, collector: &mut ReferenceCollector) {
        match self {
            TypeKind::Array(a) => {
                a.ty.assert_reference();
                match &mut a.length {
                    Ok(r) => collector.collect(r),
                    Err(_) => {}
                }
            }
            TypeKind::Container(c) => {
                for (_, f) in &mut c.fields {
                    match f {
                        crate::hir::ContainerFieldType::Normal(n) => {
                            n.assert_reference();
                            n.rewrite_references(collector)
                        }
                        crate::hir::ContainerFieldType::Virtual(r, n) => {
                            n.assert_reference();
                            n.rewrite_references(collector);
                            collector.collect(r)
                        }
                    }
                }
            }
            TypeKind::Reference(_, args) => {
                for arg in args.iter_mut() {
                    collector.collect(&mut arg.1);
                }
            }
            TypeKind::Union(u) => {
                for (_, _, c) in &mut u.matches {
                    c.assert_reference();
                    c.rewrite_references(collector);
                }
                if let Some((_, def)) = &mut u.default {
                    def.assert_reference();
                    def.rewrite_references(collector);
                }
            }
            TypeKind::Native(_) => unreachable!(),
        }
    }
    fn extract_nested_types(&mut self, receiver: &mut TypeReceiver) {
        match self {
            TypeKind::Container(c) => {
                for (field, f) in &mut c.fields {
                    receiver.push(field.clone());

                    match f {
                        crate::hir::ContainerFieldType::Normal(n) => receiver.extract(n),
                        crate::hir::ContainerFieldType::Virtual(_, t) => receiver.extract(t),
                    }

                    receiver.pop();
                }
            }
            TypeKind::Union(r) => {
                for (_, i, t) in &mut r.matches {
                    receiver.push(i.clone());
                    receiver.extract(t);
                    receiver.pop()
                }
            }
            TypeKind::Array(a) => {
                receiver.push(Ident::new("element"));
                receiver.extract(&mut a.ty);
                receiver.pop()
            }
            TypeKind::Reference(_, _) => {}
            TypeKind::Native(_) => {}
        }
    }
}

impl Type {
    fn extract_nested_types(&mut self, receiver: &mut TypeReceiver) {
        self.kind.extract_nested_types(receiver)
    }
}
pub fn extract_nested_types(id: IdGenerator, ir: &mut Namespace) {
    ir.extract_nested_types(id)
}
