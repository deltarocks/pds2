use std::collections::HashMap;

use crate::{
	ast::{self, Ident, Item, ItemArgs, Path, Statement, Value},
	hir::ContainerFieldType,
};

use super::{Namespace, ReferenceKind, Type, TypeKind};

impl TypeKind {
	fn to_ast(self) -> Vec<ast::Value> {
		match self {
			super::TypeKind::Container(c) => Item::simple_builder("container")
				.args(if c.virt {
					ItemArgs::builder().named("virtual", Item::new_simple("true").into())
				} else {
					ItemArgs::builder()
				})
				.block({
					let mut out = Vec::new();

					for (name, ty) in c.fields {
						match ty {
							ContainerFieldType::Normal(t) => out.push(ast::Statement {
								attributes: Default::default(),
								items: Item::simple_builder("field")
									.args(ItemArgs::builder().arg(Item::new_simple(name).into()))
									.then(t.to_ast()),
							}),
							ContainerFieldType::Virtual(r, t) => out.push(ast::Statement {
								attributes: Default::default(),
								items: Item::simple_builder("virtual_field")
									.args(
										ItemArgs::builder()
											.arg(Item::new_simple(name).into())
											.named("value", r.into()),
									)
									.then(t.to_ast()),
							}),
							ContainerFieldType::Delegate(to) => out.push(ast::Statement {
								attributes: Default::default(),
								items: Item::simple_builder("delegate_field")
									.args(
										ItemArgs::builder()
											.arg(Item::new_simple(name).into())
											.named("to", Item::new_simple(to).into()),
									)
									.build_values(),
							}),
						}
					}

					if let Some(to) = c.delegate_all {
						out.push(ast::Statement {
							attributes: Default::default(),
							items: Item::simple_builder("delegate_all")
								.args(ItemArgs::builder().named("to", Item::new_simple(to).into()))
								.build_values(),
						})
					}

					ast::Block {
						statements: out,
						root: false,
					}
				})
				.build_values(),
			super::TypeKind::Union(r) => Item::simple_builder("union")
				.args(match r.tag {
					crate::hir::UnionTag::LoadFrom(r) => ItemArgs::builder().named("tag", r.into()),
					crate::hir::UnionTag::Type(t) => {
						let chain = t.to_ast();
						assert_eq!(chain.len(), 1);
						ItemArgs::builder().named("tagType", chain.into_iter().next().unwrap())
					}
				})
				.block({
					let mut out = Vec::new();

					for (v, k, t) in r.matches {
						out.push(ast::Statement {
							attributes: Default::default(),
							items: Item::simple_builder("variant")
								.args(
									ItemArgs::builder()
										.arg(Item::new_simple(k).into())
										.named("match", v),
								)
								.then(t.to_ast()),
						})
					}

					if let Some((k, t)) = r.default {
						out.push(ast::Statement {
							attributes: Default::default(),
							items: Item::simple_builder("default")
								.args(ItemArgs::builder().arg(Item::new_simple(k).into()))
								.then(t.to_ast()),
						})
					}

					ast::Block {
						statements: out,
						root: false,
					}
				})
				.build_values(),
			super::TypeKind::Reference {
				ty: ReferenceKind::Path(r),
				..
			} => Item::path_builder(r).build_values(),
			super::TypeKind::Reference {
				ty: ReferenceKind::Resolved(r),
				args,
				types,
			} => {
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
							.args(ItemArgs::builder().arg(Item::new_simple(name).into()))
							.then(t.to_ast())
					},
				}),
			}
		}
		ast::Block {
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
