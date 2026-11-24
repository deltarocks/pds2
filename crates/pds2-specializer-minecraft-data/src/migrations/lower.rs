use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use pds2_core::ast::{Attributes, Block, Ident, Item, Value};
use pds2_core::definition::{LowerCtx, Lowered};

use super::rules::{parse_every, parse_migration};
use super::{MigrationsData, Version};

fn first_name(item: &Item) -> Option<String> {
	match item.args.positional().first()? {
		Value::Item(i) => Some(i.name.item.as_str().to_owned()),
		_ => None,
	}
}

fn package<'r>(root: &'r Block, name: &str) -> Option<&'r Block> {
	root.statements.iter().find_map(|s| {
		let head = s.head()?;
		(head.head_name() == Some("package") && first_name(head).as_deref() == Some(name))
			.then_some(&head.block)
	})
}

fn numbers(attributes: &Attributes, key: &str) -> Result<Vec<i32>> {
	let Some(values) = attributes.get(&Ident::new(key)) else {
		return Ok(vec![]);
	};
	values
		.iter()
		.flat_map(|p| match p {
			Value::List(items) => items.clone(),
			v => vec![v.clone()],
		})
		.map(|p| match p {
			Value::Int(n) => i32::try_from(n).map_err(|_| anyhow!("protocol {n} is out of range")),
			v => bail!("`@!{key}` lists numbers, got {v:?}"),
		})
		.collect()
}

fn snapshots(attributes: &Attributes) -> Result<Vec<(i32, String)>> {
	let Some(values) = attributes.get(&Ident::new("snapshots")) else {
		return Ok(vec![]);
	};
	values
		.iter()
		.flat_map(|p| match p {
			Value::List(items) => items.clone(),
			v => vec![v.clone()],
		})
		.map(|v| {
			let Value::Record(fields) = &v else {
				bail!("`@!snapshots` lists `(protocol: <number>, name: \"...\")`, got {v:?}");
			};
			let field = |key: &str| {
				fields
					.iter()
					.find(|(k, _)| k.as_str() == key)
					.map(|(_, v)| v)
			};
			match (field("protocol"), field("name")) {
				(Some(Value::Int(n)), Some(Value::String { string, .. })) if fields.len() == 2 => {
					Ok((
						i32::try_from(*n).map_err(|_| anyhow!("protocol {n} is out of range"))?,
						string.clone(),
					))
				}
				_ => bail!("`@!snapshots` lists `(protocol: <number>, name: \"...\")`, got {v:?}"),
			}
		})
		.collect()
}

fn defs(block: &Block, ns: &mut Vec<String>, out: &mut Vec<String>) {
	for stmt in &block.statements {
		let Some(head) = stmt.head() else { continue };
		let Some(name) = first_name(head) else {
			continue;
		};
		match head.head_name() {
			Some("namespace" | "package") => {
				ns.push(name);
				defs(&head.block, ns, out);
				ns.pop();
			}
			Some("def") => out.push(
				ns.iter()
					.chain([&name])
					.cloned()
					.collect::<Vec<_>>()
					.join("::"),
			),
			_ => {}
		}
	}
}

fn version(item: Item, ctx: &LowerCtx, first: bool) -> Result<(Version, Option<Block>)> {
	let Item { args, block, .. } = item;
	let package_name: Ident = args.arg(0, "version package")?;
	let name = match args.named::<Option<Value>>("name")? {
		Some(Value::String { string, .. }) => string,
		Some(v) => bail!("version name should be a string, got {v:?}"),
		None => bail!("version {package_name:?} needs `name: \"...\"`"),
	};
	let step = args.named::<Option<bool>>("step")?;
	args.verify_unused()?;
	let attributes = &package(ctx.root, package_name.as_str())
		.ok_or_else(|| anyhow!("version {name}: there is no package({package_name:?})"))?
		.attributes;
	let version = Version {
		package: package_name.as_str().to_owned(),
		protocols: numbers(attributes, "protocol")?,
		legacy: numbers(attributes, "legacy_protocol")?,
		snapshots: snapshots(attributes).with_context(|| format!("version {name}"))?,
		name,
	};
	if first {
		ensure!(
			block.statements.is_empty() && step.is_none(),
			"version {}: the first version has no step to migrate from",
			version.name
		);
		return Ok((version, None));
	}
	match step {
		Some(false) => {
			ensure!(
				block.statements.is_empty(),
				"version {}: a version without a step has no rules",
				version.name
			);
			Ok((version, None))
		}
		_ => Ok((version, Some(block))),
	}
}

pub fn lower(
	attributes: Attributes,
	item: Item,
	chain: Vec<Value>,
	ctx: &LowerCtx,
) -> Result<Lowered> {
	ensure!(attributes.is_empty(), "migrations take no attributes");
	ensure!(chain.is_empty(), "migrations take no `=> type`");
	let Item { args, block, .. } = item;
	let virtual_packets = args.named::<Option<bool>>("virtual")?.unwrap_or(false);
	args.verify_unused()?;

	let mut versions: Vec<Version> = Vec::new();
	let mut steps = Vec::new();
	let mut every = None;
	for statement in block.statements {
		let (attributes, item, chain) = statement.pop_item()?;
		ensure!(
			attributes.is_empty() && chain.is_empty(),
			"migrations hold `version` and `every` statements only"
		);
		match item.head_name() {
			Some("version") => {
				let (version, rules) = version(item, ctx, versions.is_empty())?;
				if !versions.is_empty() {
					let label = format!(
						"{} -> {}",
						versions.last().expect("not empty").name,
						version.name
					);
					steps.push(
						rules
							.map(|b| parse_migration(&b))
							.transpose()
							.with_context(|| label)?,
					);
				}
				ensure!(
					versions.iter().all(|v| v.package != version.package),
					"package {} is listed as two versions",
					version.package
				);
				versions.push(version);
			}
			Some("every") => {
				ensure!(every.is_none(), "`every` is given twice");
				ensure!(item.args.is_empty(), "`every` takes no arguments");
				every = Some(parse_every(&item.block).context("every step")?);
			}
			other => bail!(
				"unknown statement {:?} in migrations, expected `version` or `every`",
				other.unwrap_or_default()
			),
		}
	}

	let releases: BTreeSet<i32> = versions
		.iter()
		.flat_map(|v| v.protocols.iter().copied())
		.collect();
	let mut snapshot_names = BTreeSet::new();
	for version in &versions {
		for (protocol, name) in &version.snapshots {
			ensure!(
				!releases.contains(protocol),
				"snapshot {name} of version {}: protocol {protocol} is a release protocol",
				version.name
			);
			ensure!(
				snapshot_names.insert(name.as_str()),
				"snapshot {name} is listed twice"
			);
		}
	}

	let mut names = Vec::new();
	defs(ctx.root, &mut Vec::new(), &mut names);
	let base: BTreeMap<String, String> = names
		.into_iter()
		.map(|name| {
			let stripped = versions
				.iter()
				.find_map(|v| name.strip_prefix(&format!("{}::", v.package)))
				.unwrap_or(&name)
				.to_owned();
			(name, stripped)
		})
		.collect();
	Ok(Lowered {
		statements: vec![],
		data: Box::new(MigrationsData {
			versions,
			steps,
			every,
			virtual_packets,
			base,
		}),
	})
}
