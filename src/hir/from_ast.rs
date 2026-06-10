use std::collections::BTreeMap;

use crate::{
	ast::{
		BaseFieldRef, Block, FieldRef, FieldRefSegment, Ident, Item, ParseValue, Statement, Value,
	},
	hir::{
		BodyArg, BodyField, Container, ContainerFieldType, FieldRefOrConst, NamespaceItem,
		NativeType, NativeTypeVariant, Reference, ReferenceKind, Union, Value as HirValue,
	},
	specializer::Registry,
};

use super::{IdGenerator, Namespace, Type, TypeKind};
use anyhow::{anyhow, bail, ensure, Context, Result};

/// Parse a use-site native body (`{ field(x, size: 26); flag(a); ... }`) into [`BodyField`]s.
/// Empty for the common case of a plain reference with no block.
fn parse_specializer_body(block: &Block) -> Result<Vec<BodyField>> {
	let mut out = Vec::new();
	for stmt in &block.statements {
		let (_attrs, item, mut items) = stmt.clone().pop_item()?;
		let kind = item.name.expect_simple_name()?;
		let name: Ident = item.args.arg(0, "body field name")?;
		let mut args = BTreeMap::new();
		for n in item.args.named_names().cloned().collect::<Vec<_>>() {
			let value: Value = item.args.named(n.clone())?;
			args.insert(n, body_arg(value)?);
		}
		item.args.verify_unused()?;
		let ty = if items.is_empty() {
			None
		} else {
			let type_item = items
				.remove(0)
				.into_item()
				.ok_or_else(|| anyhow!("body field type should be item"))?;
			Some(TypeKind::from_ast(type_item, items)?)
		};
		out.push(BodyField {
			kind,
			name,
			args,
			ty,
		});
	}
	Ok(out)
}

fn body_arg(value: Value) -> Result<BodyArg> {
	Ok(match value {
		Value::Int(i) => BodyArg::Int(i),
		Value::String { string, .. } => BodyArg::Str(string),
		Value::Item(item) => match item.simple_name().map(|i| i.as_str().to_owned()).as_deref() {
			Some("true") => BodyArg::Bool(true),
			Some("false") => BodyArg::Bool(false),
			Some(other) => BodyArg::Str(other.to_owned()),
			None => bail!("unsupported body argument: {item:?}"),
		},
		other => bail!("unsupported body argument: {other:?}"),
	})
}

pub fn parse_ast(ast: Block, reg: &Registry) -> Result<Namespace> {
	let mut ns = Namespace::from_ast(ast).context("parsing AST to IR")?;
	let id = IdGenerator::new();
	super::desugar::give_all_types_ids(id.clone(), &mut ns, reg)?;

	super::desugar::rewrite_paths_to_absolute(&mut ns);
	super::desugar::resolve_paths_to_ids(&mut ns).context("resolving paths")?;

	super::desugar::apply_specializers(&mut ns, reg).context("applying specializers")?;

	super::desugar::monomorphize(id.clone(), &mut ns, reg)?;

	super::desugar::extract_nested_types(id, &mut ns, reg)?;
	super::desugar::replace_aliases(&mut ns, reg)?;
	Ok(ns)
}

impl Type {
	pub fn from_ast(value: Item, items: Vec<Value>, block: Block) -> Result<Type> {
		let mut params = Vec::new();
		let mut types = BTreeMap::new();
		for stmt in block.statements {
			let (_attrs, item, mut items) = stmt.pop_item()?;
			ensure!(items.is_empty(), "expected only one item");
			match item.name.expect_simple_name()?.as_str() {
				"argument" => {
					let name: Ident = item.args.arg(0, "argument name")?;
					item.args.verify_unused()?;

					params.push(name);
				}
				"type" => {
					let name: Ident = item.args.arg(0, "generic name")?;
					item.args.verify_unused()?;
					let default = if items.is_empty() {
						None
					} else {
						let type_item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("missing virtual field type"))?;
						Some(TypeKind::from_ast(type_item, items)?)
					};

					ensure!(
						types.insert(name.clone(), default).is_none(),
						"generic set twice: {name:?}"
					);
				}
				v => bail!("unknown def block statement type: {}", v),
			};
		}

		Ok(Type {
			_id: Default::default(),
			types,
			params,
			kind: TypeKind::from_ast(value, items)?,
		})
	}
}

impl Union {
	fn from_ast(item: Item) -> Result<TypeKind> {
		let tag = item
			.args
			.either::<Item, FieldRef>("tagType", "tag")
			.context("union tag")?;
		item.args.verify_unused()?;

		let mut fields = Vec::new();
		let mut default = None::<TypeKind>;

		for stmt in item.block.statements {
			let (_attrs, item, mut items) = stmt.pop_item()?;
			match item.name.expect_simple_name()?.as_str() {
				"variant" => {
					let name: Ident = item.args.arg(0, "field name")?;
					let value: Value = item.args.named("match")?;
					item.args.verify_unused()?;
					let value = match i128::parse(value.clone()) {
						Ok(n) => HirValue::Number(n),
						Err(_) => match value {
							Value::String { string, is_block: _ } => {
								HirValue::UnionDescriminator(Ident::new(&string))
							},
							_ => bail!("union match value should be either number on union descriminator string")
						},
					};
					if items.is_empty() {
						fields.push((value, name, TypeKind::Void));
					} else {
						let type_item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("field item should be item"))?;
						fields.push((
							value,
							name,
							TypeKind::from_ast(type_item, items).context("union item type")?,
						));
					}
				}
				"default" => {
					let v = if items.is_empty() {
						TypeKind::Void
					} else {
						let type_item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("field item should be item"))?;

						TypeKind::from_ast(type_item, items).context("union item type")?
					};
					if default.is_some() {
						bail!("union default value was set twice");
					}
					default = Some(v);
				}
				v => bail!("unknown union field type: {}", v),
			}
		}

		match tag {
			Ok(t) => Ok(TypeKind::Container(Container {
				virt: true,
				fields: vec![
					(
						Ident::new("_tag"),
						ContainerFieldType::Normal(
							TypeKind::from_ast(t, vec![]).context("union tag")?,
						),
					),
					(
						Ident::new("_value"),
						ContainerFieldType::Normal(TypeKind::Union(Self {
							tag: FieldRef {
								up: 1,
								path: BaseFieldRef(vec![FieldRefSegment::Down {
									to: Ident::new("_tag"),
									ignore_delegation: true,
								}]),
							},
							values: fields,
							default: default.map(Box::new),
						})),
					),
				],
				delegate_all: Some(Ident::new("_value")),
			})),
			Err(tag) => Ok(TypeKind::Union(Self {
				tag,
				values: fields,
				default: default.map(Box::new),
			})),
		}
		// Ok(Self{
		// 	tag:
		// })

		// match
		// {
		// 	Ok(tag_ty) => {
		// 		todo!()
		// 	}
		// 	Err(v) => {
		// 		for ele in item.block.statements {
		//
		// 		}
		// 		Ok(Self{
		// 		tag: v,
		// 		values:
		// 	})
		// 	}
		// }
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
								.with_context(|| format!("container field <{name:?}>"))?,
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
				_ => {
					// ensure!(items.is_empty(), "unexpected values");
					let body = parse_specializer_body(&value.block)?;

					let mut args = Vec::new();
					let mut types = BTreeMap::new();
					// We don't have reference resolved at this phase, so verify_unused check will only reject unused unnamed arguments
					for name in value.args.named_names() {
						if name.is_type_arg() {
							let ty: Item = value.args.named(name.clone())?;
							let kind = TypeKind::from_ast(ty, vec![])?;
							if types.insert(name.clone(), kind).is_some() {
								bail!("generic set twice: {name:?}")
							}
						} else {
							let value: Value = value
								.args
								.named(name.clone())
								.context("parsing value argument")?;
							args.push((
								name.clone(),
								match value {
									Value::FieldRef(field_ref) => FieldRefOrConst::FieldRef(field_ref),
									Value::Int(i) => FieldRefOrConst::Const(HirValue::Number(i)),
									Value::String{string, is_block: _} => FieldRefOrConst::Const(HirValue::String(string)),
									_ => bail!("reference argument should be either field reference or constant, got {value:?}"),
								},
							));
						}
					}
					value.args.verify_unused()?;

					if !items.is_empty() {
						let item = items
							.remove(0)
							.into_item()
							.ok_or_else(|| anyhow!("plain type value should be item"))?;
						let kind = TypeKind::from_ast(item, items)?;
						if types.insert(Ident::new("Value"), kind).is_some() {
							bail!("generic set twice: Value");
						}
					}

					return Ok(TypeKind::Reference(Reference {
						ty: ReferenceKind::Path(value.name),
						args,
						types,
						body,
					}));
				}
			};
			Ok(kind)
		} else {
			Ok(TypeKind::Reference(Reference {
				ty: ReferenceKind::Path(value.name),
				args: vec![],
				types: BTreeMap::new(),
				body: vec![],
			}))
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
						Field(Ident, TypeKind),
						Type(Ident, Option<TypeKind>),
						Body,

						Variant(Ident, Vec<NativeStmt>),
					}
					impl NativeStmt {
						fn from_ast(value: Statement) -> Result<Self> {
							let (_attrs, item, mut items) = value.pop_item()?;

							Ok(match item.name.expect_simple_name()?.as_str() {
								"variant" => {
									ensure!(items.is_empty(), "argument has no items");
									let id = item.args.arg(0, "variant name")?;
									item.args.verify_unused()?;

									let mut stmts = vec![];
									for ele in item.block.statements {
										let native = NativeStmt::from_ast(ele)?;
										ensure!(
											!matches!(native, NativeStmt::Variant(..)),
											"variants can't be nested"
										);
										stmts.push(native);
									}

									Self::Variant(id, stmts)
								}
								"argument" => {
									ensure!(items.is_empty(), "argument has no items");
									Self::Param(item.args.arg(0, "argument name")?)
								}
								"body" => {
									ensure!(items.is_empty(), "body has no items");
									item.args.verify_unused()?;
									Self::Body
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
								"field" => {
									let type_item = items
										.remove(0)
										.into_item()
										.ok_or_else(|| anyhow!("missing field type"))?;
									// let block = item.block;
									Self::Field(
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

					fn native_type_statements(
						stmts: Vec<NativeStmt>,
					) -> Result<(NativeTypeVariant, BTreeMap<Ident, NativeTypeVariant>)> {
						let mut params = Vec::new();
						let mut virtual_fields = Vec::new();
						let mut fields = Vec::new();
						let mut types = BTreeMap::new();
						let mut accepts_body = false;
						let mut variants = BTreeMap::new();
						for statement in stmts {
							match statement {
								NativeStmt::Param(name) => params.push(name),
								NativeStmt::VirtualField(name, ty) => {
									virtual_fields.push((name, ty))
								}
								NativeStmt::Field(name, ty) => fields.push((name, ty)),
								NativeStmt::Type(ty, default) => ensure!(
									types.insert(ty.clone(), default).is_none(),
									"generic set twice: {ty:?}"
								),
								NativeStmt::Body => accepts_body = true,
								NativeStmt::Variant(name, v) => {
									let (var, vars) = native_type_statements(v)?;
									assert!(vars.is_empty(), "checked in stmt parsing");
									variants.insert(name.clone(), var);
								}
							}
						}
						let specialization = types
							.iter()
							.map(|(n, t)| {
								(
									n.clone(),
									t.clone().unwrap_or_else(|| {
										TypeKind::Reference(Reference {
											ty: ReferenceKind::Generic(n.clone()),
											args: vec![],
											types: BTreeMap::new(),
											body: vec![],
										})
									}),
								)
							})
							.collect();
						Ok((
							NativeTypeVariant {
								virtual_fields,
								fields,
								accepts_body,
								specialization,
								params,
								types,
							},
							variants,
						))
					}

					let mut native_stmt = Vec::new();
					for statement in item.block.statements {
						let stmt = NativeStmt::from_ast(statement)?;
						native_stmt.push(stmt);
					}
					let (default, mut variants) = native_type_statements(native_stmt)?;
					if variants.is_empty() {
						variants.insert(Ident::new("default"), default);
					} else {
						ensure!(
							!variants.contains_key(&Ident::new("default")),
							"default variant name is reserved"
						);
						for (_, var) in variants.iter_mut() {
							var.extend(&default);
						}
					}

					let has_types = variants.values().any(|v| !v.types.is_empty());
					let accepts_body = variants.values().any(|v| v.accepts_body);

					let name = item.args.arg(0, "native name")?;
					ensure!(
						(!has_types && !accepts_body) == specializer.is_none(),
						"specializer should be set when the native has generics or a body in <{name:?}>"
					);

					(
						name,
						NamespaceItem::Type(Type {
							_id: Default::default(),
							kind: TypeKind::Native(NativeType {
								specializer,
								variants,
							}),
							// Native type is not generic per-se and won't be monomorphised as normal types do
							// However, it might make sense to have optional @monomorphise on generic type so the
							// actual native type is monomorphised?..
							types: BTreeMap::new(),
							params: Vec::new(),
							// types,
							// params,
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
								.with_context(|| format!("def <{:?}>", name))?,
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
