use crate::ast::{self, builder::ItemBuilder, BaseReference, Block, Item, ItemArgs, Statement};
use anyhow::{anyhow, bail, ensure, Result};
use serde_json::Value;

pub fn parse_reference(r: &str) -> Result<ast::Reference> {
    Ok(ast::Reference {
        up: 10,
        path: BaseReference(vec![(true, "todo".into())]),
    })
}

pub fn upgrade_type(value: Value) -> Result<Vec<ast::Value>> {
    match value {
        Value::Array(a) => {
            ensure!(a.len() >= 2, "array type should be 2 or more in length");
            let name = a[0]
                .as_str()
                .ok_or_else(|| anyhow!("first array value should be string"))?;
            Ok(match name {
                "entityMetadataLoop" => vec![],
                "particleData" => vec![],
                "switch" => {
                    let data = a[1]
                        .as_object()
                        .ok_or_else(|| anyhow!("mapper data should be object"))?;
                    let compare_to = data
                        .get("compareTo")
                        .ok_or_else(|| anyhow!("missing compareTo"))?
                        .as_str()
                        .ok_or_else(|| anyhow!("compareTo should be string"))?;

                    let fields = data
                        .get("fields")
                        .ok_or_else(|| anyhow!("missing fields"))?
                        .as_object()
                        .ok_or_else(|| anyhow!("fields should be object"))?;

                    Item::simple_builder("union")
                        .args(
                            ItemArgs::builder()
                                .named("tag", ast::Value::Reference(parse_reference(compare_to)?)),
                        )
                        .block({
                            let mut statements = Vec::new();

                            for (k, v) in fields {
                                statements.push(Statement {
                                    attributes: Default::default(),
                                    items: Item::simple_builder("variant")
                                        .args(
                                            ItemArgs::builder()
                                                .arg(ast::Value::new_simple_item(k.as_str()))
                                                .named(
                                                    "match",
                                                    ast::Value::new_string(k.to_string()),
                                                ),
                                        )
                                        .then(upgrade_type(v.clone())?),
                                });
                            }

                            Block {
                                statements,
                                root: false,
                            }
                        })
                        .build_values()
                }
                "bitfield" => vec![],
                "pstring" => vec![],
                "mapper" => {
                    let data = a[1]
                        .as_object()
                        .ok_or_else(|| anyhow!("mapper data should be object"))?;
                    let ty = data.get("type").ok_or_else(|| anyhow!("missing type"))?;
                    let parsed = upgrade_type(ty.clone())?;
                    ensure!(
                        parsed.len() == 1,
                        "only simple types is allowed as enum input"
                    );
                    let v = parsed[0].item().unwrap().clone();

                    let mappings = data
                        .get("mappings")
                        .ok_or_else(|| anyhow!("missing mappings"))?
                        .as_object()
                        .ok_or_else(|| anyhow!("mappings should be object"))?;

                    Item::simple_builder("union")
                        .args(ItemArgs::builder().named("tagType", ast::Value::Item(v)))
                        .block({
                            let mut statements = Vec::new();

                            for (k, v) in mappings {
                                let v = v
                                    .as_str()
                                    .ok_or_else(|| anyhow!("mapping value should be string"))?;
                                statements.push(Statement {
                                    attributes: Default::default(),
                                    items: Item::simple_builder("variant")
                                        .args(
                                            ItemArgs::builder()
                                                .arg(ast::Value::new_simple_item(v))
                                                .named(
                                                    "match",
                                                    ast::Value::new_string(k.to_string()),
                                                ),
                                        )
                                        .build_values(),
                                });
                            }

                            Block {
                                statements,
                                root: false,
                            }
                        })
                        .build_values()
                }
                "buffer" => vec![],
                "option" => vec![],
                "topBitSetTerminatedArray" => vec![],
                "array" => {
                    let data = a[1]
                        .as_object()
                        .ok_or_else(|| anyhow!("array data should be object"))?;
                    ensure!(data.len() == 2, "unknown array fields");
                    let ty = data.get("type").ok_or_else(|| anyhow!("missing type"))?;
                    if let Some(count_type) = data.get("countType") {
                        let parsed = upgrade_type(count_type.clone())?;
                        ensure!(
                            parsed.len() == 1,
                            "only simple types is allowed as array lenght"
                        );
                        let v = parsed[0].item().unwrap().clone();
                        Item::simple_builder("array")
                            .args(ItemArgs::builder().named("lengthType", ast::Value::Item(v)))
                            .build_values()
                    } else if let Some(count) = data.get("count") {
                        let reference = count.as_str().ok_or_else(|| anyhow!("bad reference"))?;
                        Item::simple_builder("array")
                            .args(ItemArgs::builder().named(
                                "length",
                                ast::Value::Reference(parse_reference(reference)?),
                            ))
                            .build_values()
                    } else {
                        bail!("unknown array kind");
                    }
                }
                "container" => Item::simple_builder("container")
                    .block({
                        let mut statements = Vec::new();
                        let data = a[1]
                            .as_array()
                            .ok_or_else(|| anyhow!("expected container data to be array"))?;
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
                                    .args(
                                        ItemArgs::builder().arg(ast::Value::new_simple_item(name)),
                                    )
                                    .then(upgrade_type(ty.clone())?),
                            })
                        }
                        Block {
                            statements,
                            root: false,
                        }
                    })
                    .build_values(),
                v => bail!("unknown type name: {:?}", v),
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
                        .args(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())))
                        .build_values(),
                })
            } else {
                statements.push(ast::Statement {
                    attributes: Default::default(),
                    items: Item::simple_builder("def")
                        .args(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())))
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
                .args(ItemArgs::builder().arg(ast::Value::new_simple_item(k.as_str())))
                .block(upgrade_namespace(v.clone())?)
                .build_values(),
        });
    }
    Ok(ast::Block {
        statements: statements,
        root: false,
    })
}

pub fn upgrade(value: Value) -> Result<ast::Block> {
    let mut block = upgrade_namespace(value)?;
    block.root = true;
    Ok(block)
}
