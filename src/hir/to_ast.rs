use std::collections::HashMap;

use crate::ast::{self, Ident, Item, ItemArgs, Path, Statement, Value};

use super::{Namespace, Reference, ReferenceKind, Type, TypeKind};

impl TypeKind {
	fn to_ast(self) -> Vec<ast::Value> {
		match self {
			super::TypeKind::Reference(Reference {
				ty: ReferenceKind::Path(r),
				..
			}) => Item::path_builder(r).build_values(),
			super::TypeKind::Reference(Reference {
				ty: ReferenceKind::Resolved(r),
				args,
				types,
				..
			}) => {
				vec![Value::Internal(format!(
					"type reference {:?} (args: {:?}, types: {:?})",
					r, args, types
				))]
			}
			TypeKind::Void => vec![],
			TypeKind::Native(_) => unreachable!(),
			_ => todo!(),
		}
	}
}

impl Type {
	fn to_ast(self) -> Vec<ast::Value> {
		self.kind.to_ast()
	}
}

impl Namespace {
	fn to_ast(self) -> ast::Block {
		let mut out = Vec::new();
		for (name, item) in self.items {
			match item {
				super::NamespaceItem::Inner(ns) => out.push(Statement {
					attributes: HashMap::new(),
					items: vec![ast::Value::Item(ast::Item {
						name: Path::new_simple(Ident::new("namespace")),
						args: ItemArgs::builder()
							.arg(Item::new_simple(name).into())
							.build(),
						block: ns.to_ast(),
					})],
				}),
				super::NamespaceItem::Type(t) => out.push(Statement {
					attributes: HashMap::new(),
					items: {
						ast::Item::simple_builder(if t.is_native() { "def_native" } else { "def" })
							.argsb(ItemArgs::builder().arg(Item::new_simple(name).into()))
							.then(t.to_ast())
					},
				}),
			}
		}
		ast::Block {
			attributes: Default::default(),
			statements: out,
			root: false,
		}
	}
}

pub fn to_ast(namespace: Namespace) -> ast::Block {
	let mut block = namespace.to_ast();
	block.root = true;
	block
}
