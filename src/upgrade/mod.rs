use crate::ast::{self, BaseFieldRef, Block, FieldRefSegment, Ident, Item, ItemArgs, Statement};
use anyhow::{anyhow, bail, ensure, Result};
use inflector::Inflector;
use serde_json::Value;

enum ProtodefPathItem {
	Down(Ident),
	Arg(Ident),
}

pub struct ProtodefPath {
	up: usize,
	items: Vec<ProtodefPathItem>,
}

peg::parser!(
grammar protodef() for str {
	rule ident() -> Ident = s:$(['a'..='z' | 'A'..='Z' | '_']+) { s.into() }
	rule path_item() -> ProtodefPathItem = arg:"$"? ident:ident() { if arg.is_some() { ProtodefPathItem::Arg(ident) } else { ProtodefPathItem::Down(ident) } }
	pub rule path() -> ProtodefPath = v:("../")* items:path_item()++"/" {ProtodefPath {up: v.len(), items}}
}
);

pub fn parse_reference(r: &str, extra_ups: usize) -> Result<ast::FieldRef> {
	let path = protodef::path(r)?;
	for i in path.items.iter().skip(1) {
		ensure!(
			!matches!(i, ProtodefPathItem::Arg(_)),
			"argument only can be first"
		)
	}
	Ok(ast::FieldRef {
		up: path.up + extra_ups,
		path: BaseFieldRef(
			path.items
				.into_iter()
				.map(|i| match i {
					ProtodefPathItem::Down(v) => FieldRefSegment::Down {
						to: v,
						ignore_delegation: false,
					},
					ProtodefPathItem::Arg(v) => FieldRefSegment::Arg(v),
				})
				.collect::<Vec<_>>(),
		),
	})
}

pub fn upgrade_value(value: Value) -> Result<ast::Value> {
	match value {
		Value::Number(n) => Ok(ast::Value::Int(
			n.as_i64().ok_or_else(|| anyhow!("bad integer"))? as i128,
		)),
		_ => bail!("unknown value type"),
	}
}

pub fn upgrade_type(value: Value) -> Result<Vec<ast::Value>> {
	match value {
		Value::Array(a) => {
			ensure!(a.len() >= 2, "array type should be 2 or more in length");
			let name = a[0]
				.as_str()
				.ok_or_else(|| anyhow!("first array value should be string: {a:?}"))?;
			Ok(match name {
				"switch" | "mapper" => bail!("switch/mapper are disallowed"),
				"container" => Item::simple_builder("container")
					.block({
						let mut statements = Vec::new();
						let mut data = a[1]
							.as_array()
							.ok_or_else(|| anyhow!("expected container data to be array"))?
							.clone();
						// fix_container_union(&mut data);
						for item in data {
							let item = item
								.as_object()
								.ok_or_else(|| anyhow!("object field should be object"))?;
							ensure!(item.len() == 2, "object field has only two fields");
							println!("{:?}", item);
							let name = if item.contains_key("anon") {
								"ANON"
							} else {
								item.get("name")
									.ok_or_else(|| anyhow!("missing name"))?
									.as_str()
									.ok_or_else(|| anyhow!("expected name to be string"))?
							};
							let ty = item.get("type").ok_or_else(|| anyhow!("missing type"))?;

							statements.push(Statement {
								attributes: Default::default(),
								items: Item::simple_builder("field")
									.argsb(
										ItemArgs::builder().arg(ast::Value::new_simple_item(name)),
									)
									.then(upgrade_type(ty.clone())?),
							})
						}
						Block {
							attributes: Default::default(),
							statements,
							root: false,
						}
					})
					.build_values(),
				"bitfield" => Item::simple_builder("container").build_values(),
				f if a.len() == 2 && a[1].is_string() => {
					Item::simple_builder(f).then(upgrade_type(a[1].clone())?)
				}
				f if a.len() == 2 && a[1].is_array() => {
					Item::simple_builder(f).then(upgrade_type(a[1].clone())?)
				}
				f if a.len() == 2 && a[1].is_object() => {
					let obj = a[1].as_object().unwrap();
					let mut builder = Item::simple_builder(f);
					let mut args = ItemArgs::builder();
					let mut value = None;

					for (k, v) in obj {
						if k == "type" {
							value = Some(v.clone())
						} else if let Some(val) = k.strip_suffix("Val") {
							args = args.named(val, upgrade_value(v.clone())?);
						} else if let Some(val) = k.strip_suffix("Type") {
							let ty = upgrade_type(v.clone())?;
							if ty.len() != 1 {
								bail!("only primitives for now")
							}
							let val = Inflector::to_pascal_case(val);
							args = args.named(val.as_str(), ty[0].clone());
						} else if let Some(val) = k.strip_suffix("To") {
							args = args.named(
								val,
								ast::Value::FieldRef(parse_reference(
									v.as_str().ok_or_else(|| anyhow!("bad reference value"))?,
									1,
								)?),
							);
						} else {
							bail!("unexpected key: {k:?} => {v:?}");
						}
					}
					let args = args.build();
					if !args.is_empty() {
						builder = builder.args(args);
					}

					let mut o = builder.build_values();
					if let Some(value) = value {
						o.extend(upgrade_type(value)?);
					}
					o
				}
				v => bail!("unknown type name: {:?} => {:?}", v, a),
			})
		}
		Value::String(s) => Ok(Item::simple_builder(&s as &str).build_values()),
		v => bail!("can't understand type: {:?}", v),
	}
}

pub fn upgrade_namespace(value: Value) -> Result<ast::Block> {
	let mut statements = Vec::new();
	ensure!(value.is_object(), "namespace should be object");
	if let Some(types) = value.get("types") {
		for (k, v) in types.as_object().unwrap() {
			if v == "native" {
				statements.push(ast::Statement {
					attributes: Default::default(),
					items: Item::simple_builder("def_native")
						.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())))
						.build_values(),
				})
			} else {
				statements.push(ast::Statement {
					attributes: Default::default(),
					items: Item::simple_builder("def")
						.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())))
						.then(upgrade_type(v.clone())?),
				})
			}
		}
	}
	for (k, v) in value.as_object().unwrap().into_iter() {
		if k == "types" {
			continue;
		}
		statements.push(ast::Statement {
			attributes: Default::default(),
			items: Item::simple_builder("namespace")
				.argsb(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())))
				.block(upgrade_namespace(v.clone())?)
				.build_values(),
		});
	}
	Ok(ast::Block {
		attributes: Default::default(),
		statements,
		root: false,
	})
}

pub fn upgrade(value: Value) -> Result<ast::Block> {
	let mut block = upgrade_namespace(value)?;
	block.root = true;
	Ok(block)
}
