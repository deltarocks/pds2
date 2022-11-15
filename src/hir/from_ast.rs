use std::{collections::BTreeMap, convert::TryInto};

use crate::{
	ast::{Block, Ident, Item, ParseValue, Reference, Statement, Value},
	hir::{
		Array, ArrayLength, Container, ContainerFieldType, NamespaceItem, NativeType,
		ReferenceKind, Union,
	},
};

use super::{IdGenerator, Namespace, Type, TypeKind, UnionTag};
use anyhow::{anyhow, bail, ensure, Context, Result};

pub fn parse_ast(ast: Block) -> Result<Namespace> {
	let mut ns = Namespace::from_ast(ast).context("parsing AST to IR")?;
	let id = IdGenerator::new();
	super::desugar::rewrite_paths_to_absolute(&mut ns);
	super::desugar::extract_nested_types(id.clone(), &mut ns);
	super::desugar::give_all_types_ids(id.clone(), &mut ns);
	super::desugar::resolve_paths_to_ids(&mut ns).context("resolving paths")?;
	super::desugar::monomorphize(id, &mut ns);
	Ok(ns)
}

impl Type {
	pub fn from_ast(value: Item, items: Vec<Value>, block: Block) -> Result<Type> {
		let mut params = Vec::new();
		for stmt in block.statements {
			let (_attrs, item, items) = stmt.pop_item()?;
			ensure!(items.is_empty(), "expected only one item");
			match item.name.expect_simple_name()?.as_str() {
				"argument" => {
					let name: Ident = item.args.arg(0, "argument name")?;
					item.args.verify_unused()?;

					params.push(name);
				}
				v => bail!("unknown def block statement type: {}", v),
			};
		}

		Ok(Type {
			_id: Default::default(),
			types: Vec::new(),
			params,
			kind: TypeKind::from_ast(value, items)?,
		})
	}
}

impl Container {
	fn from_ast(item: Item) -> Result<Self> {
		let virt = item.args.named("virtual").unwrap_or(false);
		item.args.verify_unused()?;

		let mut fields = Vec::new();

		for stmt in item.block.statements {
			let (_attrs, item, mut items) = stmt.pop_item()?;
			let (name, field) = match item.name.expect_simple_name()?.as_str() {
				"field" => {
					let name: Ident = item.args.arg(0, "field name")?;
					item.args.verify_unused()?;

					if items.is_empty() {
						bail!("no field type")
					}
					let type_item = items
						.remove(0)
						.into_item()
						.ok_or_else(|| anyhow!("field item should be item"))?;
					(
						name.clone(),
						ContainerFieldType::Normal(
							TypeKind::from_ast(type_item, items)
								.with_context(|| format!("container field {:?}", name))?,
						),
					)
				}
				"virtual_field" => {
					let name: Ident = item.args.arg(0, "virtual field name")?;
					let value = item.args.named("value")?;
					item.args.verify_unused()?;

					let type_item = items
						.remove(0)
						.into_item()
						.ok_or_else(|| anyhow!("field item should be item"))?;
					(
						name,
						ContainerFieldType::Virtual(value, TypeKind::from_ast(type_item, items)?),
					)
				}
				v => bail!("unknown container field type: {}", v),
			};
			fields.push((name, field))
		}

		Ok(Container {
			virt,
			fields,
			delegate_all: None,
		})
	}
}

impl Union {
	fn from_ast_with_tag(tag: UnionTag, value: Item) -> Result<Self> {
		let mut matches = Vec::new();
		let mut default = None;

		for stmt in value.block.statements {
			let (_attrs, item, mut items) = stmt.pop_item()?;
			match item.name.expect_simple_name()?.as_str() {
				"variant" => {
					let name: Ident = item.args.arg(0, "variant name")?;
					let value = item.args.named("match")?;
					if items.is_empty() {
						matches.push((value, name.clone(), TypeKind::Void));
					// bail!("union field has no type");
					} else {
						let type_item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("field item should be item"))?;
						matches.push((
							value,
							name.clone(),
							TypeKind::from_ast(type_item, items)
								.with_context(|| format!("union {:?} field", name))?,
						));
					}
				}
				"default" => {
					let name: Ident = item.args.arg(0, "default name")?;
					let type_item = items
						.remove(0)
						.into_item()
						.ok_or_else(|| anyhow!("field item should be item"))?;
					ensure!(
						default
							.replace((
								name.clone(),
								Box::new(TypeKind::from_ast(type_item, items).with_context(
									|| format!("default ({:?}) union field", name)
								)?)
							))
							.is_none(),
						"default is already set"
					)
				}
				v => bail!("unknown union field type: {}", v),
			};
		}

		Ok(Union {
			tag,
			matches,
			default,
		})
	}
	fn from_ast(value: Item) -> Result<TypeKind> {
		match value.args.named("tagType")? {
			Some(t) => {
				return Ok(TypeKind::Container(Container {
					virt: true,
					fields: vec![
						(
							"_tag".into(),
							ContainerFieldType::Virtual(
								Reference::parse("./union/@tag").unwrap(),
								TypeKind::from_ast(t, vec![])?,
							),
						),
						(
							"union".into(),
							ContainerFieldType::Normal(
								Self::from_ast_with_tag(
									UnionTag::LoadFrom(Reference::parse("./_tag").unwrap()),
									value,
								)
								.map(TypeKind::Union)?,
							),
						),
					],
					delegate_all: Some("union".into()),
				}));
			}
			None => {}
		};
		let tag = match value.args.named("tag")? {
			Some(r) => UnionTag::LoadFrom(r),
			None => bail!("neither tag nor tagType is specified"),
		};
		Self::from_ast_with_tag(tag, value).map(TypeKind::Union)
	}
}

impl Array {
	fn from_ast_with_length(
		length: ArrayLength,
		_value: Item,
		mut items: Vec<Value>,
	) -> Result<Self> {
		let type_item = items
			.remove(0)
			.into_item()
			.ok_or_else(|| anyhow!("array item should be item"))?;

		Ok(Array {
			length,
			ty: Box::new(TypeKind::from_ast(type_item, items)?),
		})
	}
	fn from_ast(value: Item, items: Vec<Value>) -> Result<TypeKind> {
		match value.args.named("lengthType")? {
			Some(t) => {
				return Ok(TypeKind::Container(Container {
					virt: true,
					fields: vec![
						(
							"_size".into(),
							ContainerFieldType::Virtual(
								Reference::parse("./._array/@size").unwrap(),
								TypeKind::from_ast(t, vec![])?,
							),
						),
						(
							"_array".into(),
							ContainerFieldType::Normal(
								Self::from_ast_with_length(
									ArrayLength::LoadFrom(Reference::parse("../._size").unwrap()),
									value,
									items,
								)
								.map(TypeKind::Array)?,
							),
						),
					],
					delegate_all: Some("_array".into()),
				}));
			}
			None => {}
		};
		let length = match value.args.named("length")? {
			Value::Reference(r) => ArrayLength::LoadFrom(r),
			Value::Int(v) => ArrayLength::Fixed(
				v.try_into()
					.map_err(|_| anyhow!("bad length for fixed-sized array"))?,
			),
			Value::Unset => bail!("neigher length nor lengthType is set for array"),
			v => bail!("array length should be reference or int, got {:?}", v),
		};
		Self::from_ast_with_length(length, value, items).map(TypeKind::Array)
	}
}

impl TypeKind {
	fn from_ast(value: Item, mut items: Vec<Value>) -> Result<Self> {
		if let Some(simple) = value.name.simple_name() {
			let kind = match simple.as_str() {
				"container" => {
					ensure!(items.is_empty(), "container has no items");
					TypeKind::Container(Container::from_ast(value).context("parsing container")?)
				}
				"union" => {
					ensure!(items.is_empty(), "union has no items");
					Union::from_ast(value).context("parsing union")?
				}
				"array" => Array::from_ast(value, items).context("parsing array")?,
				// "option" => TypeKind::Union(Union {
				// 	tag: UnionTag::Type(Box::new(TypeKind::Native(NativeType::Uint(1)))),
				// 	// TODO: fill variants
				// 	matches: vec![],
				// 	default: None,
				// }),
				_ => {
					// ensure!(items.is_empty(), "unexpected values");
					ensure!(value.block.is_empty(), "unexpected block");

					let mut args = Vec::new();
					let mut types = Vec::new();
					// We don't have reference resolved at this phase, so verify_unused check will only reject unused unnamed arguments
					for name in value.args.named_names() {
						if name.is_type_arg() {
							let ty: Item = value.args.named(name.clone())?;
							let kind = TypeKind::from_ast(ty, vec![])?;
							types.push((name.clone(), kind));
						} else {
							args.push((name.clone(), value.args.named(name.clone())?));
						}
					}
					value.args.verify_unused()?;

					if !items.is_empty() {
						let item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("plain type value should be item"))?;
						let kind = TypeKind::from_ast(item, items)?;
						types.push((Ident::new("Value"), kind));
					}

					return Ok(TypeKind::Reference {
						ty: ReferenceKind::Path(value.name),
						args,
						types,
					});
				}
			};
			Ok(kind)
		} else {
			Ok(TypeKind::Reference {
				ty: ReferenceKind::Path(value.name),
				args: vec![],
				types: vec![],
			})
		}
	}
}

impl Namespace {
	fn from_ast(value: Block) -> Result<Self> {
		let mut out = BTreeMap::new();
		for statement in value.statements {
			let (attrs, item, mut items) = statement.pop_item()?;

			let (name, item) = match item.name.expect_simple_name()?.as_str() {
				"namespace" => {
					ensure!(items.is_empty(), "namespace has no items");
					let name = item.args.arg(0, "ns name")?;
					let ns = Namespace::from_ast(item.block)
						.with_context(|| format!("namespace {:?}", name))?;
					(name, NamespaceItem::Inner(ns))
				}
				"def_native" => {
					// TODO: Parse @type, arguments
					ensure!(items.is_empty(), "def_native has no items");
					// ensure!(item.args.is_empty(), "args not supported");

					let specializer = if let Some(specs) = attrs.get(&Ident::new("specializer")) {
						ensure!(specs.len() == 1, "only one specializer is allowed");
						let spec = specs.get(0).unwrap();
						let specializer = Ident::parse(spec.clone())?;
						Some(specializer)
					} else {
						None
					};

					enum NativeStmt {
						Param(Ident),
						VirtualField(Ident, TypeKind),
						Type(Ident, Option<TypeKind>),
					}
					impl NativeStmt {
						fn from_ast(value: Statement) -> Result<Self> {
							let (_attrs, item, mut items) = value.pop_item()?;

							Ok(match item.name.expect_simple_name()?.as_str() {
								"argument" => {
									ensure!(items.is_empty(), "argument has no items");
									Self::Param(item.args.arg(0, "argument name")?)
								}
								"virtual_field" => {
									let type_item = items
										.remove(0)
										.into_item()
										.ok_or_else(|| anyhow!("missing virtual field type"))?;
									// let block = item.block;
									Self::VirtualField(
										item.args.arg(0, "field name")?,
										TypeKind::from_ast(type_item, items)?,
									)
								}
								"type" => {
									let default = if items.is_empty() {
										None
									} else {
										let type_item = items
											.remove(0)
											.into_item()
											.ok_or_else(|| anyhow!("missing virtual field type"))?;
										Some(TypeKind::from_ast(type_item, items)?)
									};
									Self::Type(item.args.arg(0, "type name")?, default)
								}
								n => {
									bail!("unknown native statement: {n:?}")
								}
							})
						}
					}

					let mut params = Vec::new();
					let mut virtual_fields = Vec::new();
					let mut types = Vec::new();
					for statement in item.block.statements {
						let stmt = NativeStmt::from_ast(statement)?;
						match stmt {
							NativeStmt::Param(name) => params.push(name),
							NativeStmt::VirtualField(name, ty) => virtual_fields.push((name, ty)),
							NativeStmt::Type(ty, default) => types.push((ty, default)),
						}
					}

					ensure!(
						types.is_empty() == specializer.is_none(),
						"specializer should be set when setting native types"
					);

					let name = item.args.arg(0, "native name")?;
					(
						name,
						NamespaceItem::Type(Type {
							_id: Default::default(),
							types,
							params,
							kind: TypeKind::Native(NativeType {
								specializer,
								virtual_fields,
								specialization: Vec::new(),
							}),
						}),
					)
				}
				"def" => {
					ensure!(!items.is_empty(), "def has no items");
					let name: Ident = item.args.arg(0, "def name")?;
					let type_item = items
						.remove(0)
						.into_item()
						.ok_or_else(|| anyhow!("type def should be item"))?;
					let block = item.block;
					(
						name.clone(),
						NamespaceItem::Type(
							Type::from_ast(type_item, items, block)
								.with_context(|| format!("def {:?}", name))?,
						),
					)
				}
				n => bail!("unknownn top level block statement: {:?}", n),
			};
			ensure!(
				out.insert(name.to_owned(), item).is_none(),
				"item is already defined: {:?}",
				name
			);
		}
		Ok(Self { items: out })
	}
}
