use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use pds2_core::ast::{Attributes, Block, Ident, Item, ItemArgs, Path, Statement, Value};
use pds2_core::definition::{LowerCtx, Lowered};

use super::{Method, MethodKind, ServiceData};

const LOWERED: &[&str] = &["Request", "Response"];

fn item(head: &str, arg: &str, block: Block) -> Item {
	Item::simple_builder(head)
		.argsb(ItemArgs::builder().arg(Value::new_simple_item(arg)))
		.block(block)
		.build()
}

fn exported() -> Block {
	Block {
		statements: vec![Statement::simple(vec![Value::new_simple_item("export")])],
		..Default::default()
	}
}

fn check_shadowing(value: &Value) -> Result<()> {
	match value {
		Value::Item(item) => {
			if let Some(name) = item.head_name().filter(|n| LOWERED.contains(n)) {
				bail!(
					"`{name}` would refer to the method's own lowered type here, qualify the path"
				);
			}
			for (_, v) in item.args.ordered() {
				check_shadowing(v)?;
			}
			for statement in &item.block.statements {
				for v in &statement.items {
					check_shadowing(v)?;
				}
			}
			Ok(())
		}
		Value::List(items) => items.iter().try_for_each(check_shadowing),
		Value::Record(fields) => fields.iter().try_for_each(|(_, v)| check_shadowing(v)),
		Value::Binary(_, a, b) => {
			check_shadowing(a)?;
			check_shadowing(b)
		}
		_ => Ok(()),
	}
}

pub fn lower(
	attributes: Attributes,
	service: Item,
	chain: Vec<Value>,
	ctx: &LowerCtx,
) -> Result<Lowered> {
	let Item { args, block, .. } = service;
	let name: Ident = args.arg(0, "service name")?;
	args.verify_unused()?;
	ensure!(chain.is_empty(), "service takes no `=> type`");
	let Block {
		attributes: doc,
		statements,
		..
	} = block;

	let service_ns: Vec<Ident> = ctx
		.namespace
		.iter()
		.cloned()
		.chain([name.clone()])
		.collect();
	let mut methods = Vec::new();
	let mut lowered = Vec::new();
	let mut seen = BTreeSet::new();
	let mut ids: BTreeMap<u64, Ident> = BTreeMap::new();
	for statement in statements {
		let (attrs, method_item, chain) = statement
			.pop_item()
			.context("expected `request` or `notification`")?;
		let kind_name = method_item
			.head_name()
			.ok_or_else(|| anyhow!("expected `request` or `notification`"))?
			.to_owned();
		let Item { args, block, .. } = method_item;
		let method: Ident = args.arg(0, "method name")?;
		let label = format!("{kind_name} {method}");
		let kind = match kind_name.as_str() {
			"request" => MethodKind::Request {
				cancel: args.named::<Option<bool>>("cancel")?.unwrap_or(false),
			},
			"notification" => {
				ensure!(chain.is_empty(), "notification {method} has no response");
				MethodKind::Notification {
					blocking: args.named::<Option<bool>>("blocking")?.unwrap_or(false),
				}
			}
			other => bail!(
				"unknown statement `{other}` in a service, expected `request` or `notification`"
			),
		};
		let id: Option<i128> = args.named("id")?;
		let id = id.ok_or_else(|| anyhow!("`{label}` needs an explicit `id: <number>`"))?;
		let id = u64::try_from(id).map_err(|_| anyhow!("`{label}`: id {id} is not a u64"))?;
		args.verify_unused()
			.with_context(|| format!("in `{label}`"))?;
		ensure!(
			method.as_str() != "method",
			"`{label}`: the name `method` is taken by the generated id constants"
		);
		ensure!(
			seen.insert(method.clone()),
			"method {method} is defined twice"
		);
		if let Some(other) = ids.insert(id, method.clone()) {
			bail!("method id {id} is used by both {other} and {method}");
		}
		for statement in &block.statements {
			for v in &statement.items {
				check_shadowing(v).with_context(|| format!("in `{label}`"))?;
			}
		}
		for v in &chain {
			check_shadowing(v).with_context(|| format!("in the response of `{label}`"))?;
		}

		let method_ns: Vec<Ident> = service_ns.iter().cloned().chain([method.clone()]).collect();
		let path = |item: &str| Path {
			absolute: true,
			path: method_ns.clone(),
			item: Ident::new(item),
		};
		let mut defs = vec![Statement::simple(vec![
			Value::Item(item("def", "Request", Block::default())),
			Value::Item(Item::simple_builder("container").block(block).build()),
		])];
		let response = if chain.is_empty() {
			None
		} else {
			defs.push(Statement::simple(
				std::iter::once(Value::Item(item("def", "Response", exported())))
					.chain(chain)
					.collect(),
			));
			Some(path("Response"))
		};
		methods.push(Method {
			name: method.clone(),
			id,
			kind,
			request: path("Request"),
			response,
		});
		lowered.push(Statement {
			attributes: attrs,
			items: vec![Value::Item(item(
				"namespace",
				method.as_str(),
				Block {
					statements: defs,
					..Default::default()
				},
			))],
		});
	}
	ensure!(!methods.is_empty(), "service {name} has no methods");

	let full_name = service_ns
		.iter()
		.map(Ident::as_str)
		.collect::<Vec<_>>()
		.join("::");
	Ok(Lowered {
		statements: vec![Statement {
			attributes,
			items: vec![Value::Item(item(
				"namespace",
				name.as_str(),
				Block {
					attributes: doc,
					statements: lowered,
					root: false,
				},
			))],
		}],
		data: Box::new(ServiceData {
			name,
			namespace: ctx.namespace.to_vec(),
			full_name,
			methods,
		}),
	})
}
