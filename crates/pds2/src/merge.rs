use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use pds2_core::ast::{Attributes, Block, Ident, Item, ItemArgs, Path, Statement, Value};

use crate::survey;

struct Source {
	ns: Vec<String>,
	stmt: Statement,
	native: bool,
}

fn first_name(item: &Item) -> Option<String> {
	match item.args.positional().first()? {
		Value::Item(i) => Some(i.name.item.as_str().to_owned()),
		_ => None,
	}
}

fn sources(block: &Block, ns: &mut Vec<String>, out: &mut BTreeMap<String, Source>) {
	for stmt in &block.statements {
		let Some(Value::Item(head)) = stmt.items.first() else {
			continue;
		};
		let Some(name) = first_name(head) else {
			continue;
		};
		match head.name.item.as_str() {
			"namespace" => {
				ns.push(name);
				sources(&head.block, ns, out);
				ns.pop();
			}
			kind @ ("def" | "def_native") => {
				let full = ns
					.iter()
					.chain([&name])
					.cloned()
					.collect::<Vec<_>>()
					.join("::");
				out.insert(
					full,
					Source {
						ns: ns.clone(),
						stmt: stmt.clone(),
						native: kind == "def_native",
					},
				);
			}
			_ => {}
		}
	}
}

pub fn tag(version: &str) -> String {
	format!("v{}", version.replace(['.', '-'], "_"))
}

fn resolve(names: &BTreeMap<String, Source>, ns: &[String], p: &Path) -> Option<String> {
	let tail: Vec<String> = p
		.path
		.iter()
		.chain([&p.item])
		.map(|i| i.as_str().to_owned())
		.collect();
	let prefixes = if p.absolute { 0..=0 } else { 0..=ns.len() };
	prefixes.rev().find_map(|k| {
		let candidate = ns[..k]
			.iter()
			.chain(&tail)
			.cloned()
			.collect::<Vec<_>>()
			.join("::");
		names.contains_key(&candidate).then_some(candidate)
	})
}

fn absolute(full: &str) -> Path {
	let mut parts: Vec<Ident> = full.split("::").map(Ident::new).collect();
	let item = parts.pop().expect("non-empty path");
	Path {
		absolute: true,
		path: parts,
		item,
	}
}

fn simple(name: &str) -> Value {
	Value::Item(Item {
		name: Path {
			absolute: false,
			path: vec![],
			item: Ident::new(name),
		},
		args: ItemArgs::default(),
		block: Block::default(),
	})
}

fn rewrite_value(v: &mut Value, map: &dyn Fn(&Path) -> Option<Path>, kind: bool) {
	match v {
		Value::Item(item) => {
			if kind {
				for (_, a) in item.args.named_values_mut() {
					rewrite_value(a, map, false);
				}
			} else {
				if let Some(p) = map(&item.name) {
					item.name = p;
				}
				for a in item.args.values_mut() {
					rewrite_value(a, map, false);
				}
			}
			rewrite_block(&mut item.block, map);
		}
		Value::List(items) => items.iter_mut().for_each(|x| rewrite_value(x, map, false)),
		Value::Record(fields) => fields
			.iter_mut()
			.for_each(|(_, x)| rewrite_value(x, map, false)),
		Value::Binary(_, a, b) => {
			rewrite_value(a, map, false);
			rewrite_value(b, map, false);
		}
		_ => {}
	}
}

fn rewrite_block(block: &mut Block, map: &dyn Fn(&Path) -> Option<Path>) {
	for stmt in &mut block.statements {
		for (i, v) in stmt.items.iter_mut().enumerate() {
			rewrite_value(v, map, i == 0);
		}
	}
}

#[derive(Default)]
struct Ns {
	attributes: Attributes,
	statements: Vec<Statement>,
	inner: BTreeMap<String, Ns>,
}

impl Ns {
	fn at(&mut self, path: &[String]) -> &mut Ns {
		path.iter()
			.fold(self, |ns, p| ns.inner.entry(p.clone()).or_default())
	}
	fn into_block(self, root: bool) -> Block {
		let mut statements = self.statements;
		for (name, ns) in self.inner {
			statements.push(Statement::simple(vec![Value::Item(Item {
				name: Path {
					absolute: false,
					path: vec![],
					item: Ident::new("namespace"),
				},
				args: ItemArgs::builder().arg(simple(&name)).build(),
				block: ns.into_block(false),
			})]));
		}
		Block {
			attributes: self.attributes,
			statements,
			root,
		}
	}
}

pub struct Merged {
	pub block: Block,
	pub variants: BTreeMap<String, Vec<(String, Vec<String>)>>,
}

pub fn merge(inputs: &[(String, Block)]) -> Result<Merged> {
	let versions: Vec<survey::Version> = inputs.iter().map(|(n, b)| survey::load(n, b)).collect();
	let deeps: Vec<BTreeMap<String, u64>> = versions.iter().map(survey::deep).collect();
	let srcs: Vec<BTreeMap<String, Source>> = inputs
		.iter()
		.map(|(_, b)| {
			let mut out = BTreeMap::new();
			sources(b, &mut Vec::new(), &mut out);
			out
		})
		.collect();

	let mut shapes: BTreeMap<String, Vec<(u64, usize)>> = BTreeMap::new();
	for (vi, deep) in deeps.iter().enumerate() {
		for (name, h) in deep {
			let list = shapes.entry(name.clone()).or_default();
			if !list.iter().any(|(x, _)| x == h) {
				list.push((*h, vi));
			}
		}
	}
	for (name, list) in &shapes {
		let (_, first) = list[0];
		if list.len() > 1 && srcs[first][name].native {
			bail!("native {name} differs between versions, natives can't have variants");
		}
	}
	if inputs.is_empty() {
		bail!("nothing to merge");
	}
	let home = |name: &str, h: u64| -> Option<String> {
		let list = &shapes[name];
		let (_, first) = list
			.iter()
			.find(|(x, _)| *x == h)
			.expect("hash of this name");
		(!srcs[*first][name].native).then(|| tag(&inputs[*first].0))
	};
	let variant = |name: &str, h: u64| -> String {
		match home(name, h) {
			Some(module) => format!("{module}::{name}"),
			None => name.to_owned(),
		}
	};

	let mut root = Ns::default();
	let mut variants: BTreeMap<String, Vec<(String, Vec<String>)>> = BTreeMap::new();
	for (name, list) in &shapes {
		for (h, first) in list {
			let src = &srcs[*first][name];
			let deep = &deeps[*first];
			let map = |p: &Path| {
				let full = resolve(&srcs[*first], &src.ns, p)?;
				Some(absolute(&variant(&full, deep[&full])))
			};
			let mut stmt = src.stmt.clone();
			for (i, v) in stmt.items.iter_mut().enumerate() {
				if i == 0 {
					let Value::Item(head) = v else { continue };
					rewrite_block(&mut head.block, &map);
				} else {
					rewrite_value(v, &map, false);
				}
			}
			let ns: Vec<String> = home(name, *h)
				.into_iter()
				.chain(src.ns.iter().cloned())
				.collect();
			root.at(&ns).statements.push(stmt);
			let used: Vec<String> = deeps
				.iter()
				.enumerate()
				.filter(|(_, d)| d.get(name) == Some(h))
				.map(|(vi, _)| inputs[vi].0.clone())
				.collect();
			variants
				.entry(name.clone())
				.or_default()
				.push((variant(name, *h), used));
		}
	}

	for (vi, (version, _)) in inputs.iter().enumerate() {
		for (name, src) in &srcs[vi] {
			if src.native {
				continue;
			}
			let target = variant(name, deeps[vi][name]);
			if target == format!("{}::{name}", tag(version)) {
				continue;
			}
			let item = name.rsplit("::").next().expect("non-empty name");
			let mut def = Item {
				name: Path {
					absolute: false,
					path: vec![],
					item: Ident::new("def"),
				},
				args: ItemArgs::builder().arg(simple(item)).build(),
				block: Block::default(),
			};
			def.block
				.statements
				.push(Statement::simple(vec![simple("export")]));
			let alias = Statement::simple(vec![
				Value::Item(def),
				Value::Item(Item {
					name: absolute(&target),
					args: ItemArgs::default(),
					block: Block::default(),
				}),
			]);
			let ns: Vec<String> = std::iter::once(tag(version))
				.chain(src.ns.iter().cloned())
				.collect();
			root.at(&ns).statements.push(alias);
		}
	}
	root.attributes.insert(
		Ident::new("versions"),
		inputs
			.iter()
			.map(|(v, _)| Value::String {
				string: v.clone(),
				is_block: false,
			})
			.collect(),
	);
	let doc = |string: String| {
		vec![Value::String {
			string,
			is_block: false,
		}]
	};
	let (first, last) = (&inputs[0].0, &inputs[inputs.len() - 1].0);
	root.attributes.insert(
		Ident::new("doc"),
		doc(format!(
			"Minecraft protocol types of versions {first} to {last}"
		)),
	);
	for (version, block) in inputs {
		let ns = root.at(&[tag(version)]);
		ns.attributes.insert(
			Ident::new("doc"),
			doc(format!("Minecraft {version} protocol types")),
		);
		for key in ["protocol", "legacy_protocol"] {
			if let Some(protocols) = block.attributes.get(&Ident::new(key)) {
				ns.attributes.insert(Ident::new(key), protocols.clone());
			}
		}
	}
	Ok(Merged {
		block: root.into_block(true),
		variants,
	})
}

pub struct Version {
	pub name: String,
	pub protocols: Vec<i32>,
	pub legacy: Vec<i32>,
}

fn numbers(block: &Block, key: &str) -> Result<Vec<i32>> {
	let Some(values) = block.attributes.get(&Ident::new(key)) else {
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

pub fn versions(merged: &Block) -> Result<Vec<Version>> {
	let names = merged
		.attributes
		.get(&Ident::new("versions"))
		.ok_or_else(|| anyhow!("not a merged schema: no `@!versions`"))?;
	names
		.iter()
		.map(|v| {
			let Value::String { string, .. } = v else {
				bail!("`@!versions` lists strings, got {v:?}");
			};
			let tag = tag(string);
			let ns = merged
				.statements
				.iter()
				.find_map(|s| match s.items.first() {
					Some(Value::Item(i))
						if i.name.item.as_str() == "namespace"
							&& first_name(i).as_deref() == Some(tag.as_str()) =>
					{
						Some(i)
					}
					_ => None,
				});
			let (protocols, legacy) = match ns {
				Some(ns) => (
					numbers(&ns.block, "protocol")?,
					numbers(&ns.block, "legacy_protocol")?,
				),
				None => (vec![], vec![]),
			};
			Ok(Version {
				name: string.clone(),
				protocols,
				legacy,
			})
		})
		.collect()
}
