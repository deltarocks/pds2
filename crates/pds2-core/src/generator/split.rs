use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::hir::TypeId;
use crate::mir::TypeMap;
use crate::specializer::Registry;

use super::naming::Modules;
use super::views;
use super::GeneratorOutput;

#[derive(Clone, Debug)]
pub struct Split {
	pub name: String,
	pub prelude: String,
	pub dependencies: Vec<String>,
	pub root_doc: Option<String>,
	pub namespace_docs: BTreeMap<String, String>,
	pub versions: BTreeMap<String, String>,
}

impl Split {
	fn version(&self, package: &str) -> &str {
		self.versions.get(package).map_or("0.1.0", String::as_str)
	}
}

pub(super) struct Crate {
	pub package: String,
	pub ident: String,
	pub ns: Option<String>,
	pub namespace: Option<String>,
}

impl Crate {
	fn new(package: String, ns: Option<String>, namespace: Option<String>) -> Self {
		Self {
			ident: package.replace('-', "_"),
			package,
			ns,
			namespace,
		}
	}
	pub fn file(&self, path: &str) -> PathBuf {
		PathBuf::from(format!("{}/src/{path}", self.package))
	}
}

pub(super) const ROOT: usize = 0;

pub(super) struct Layout {
	pub umbrella: String,
	pub crates: Vec<Crate>,
	pub of: BTreeMap<TypeId, usize>,
	pub strip: BTreeSet<TypeId>,
}

impl Layout {
	pub fn new(types: &TypeMap, reg: &Registry, modules: &Modules, name: &str) -> Result<Self> {
		let mut crates = vec![Crate::new(format!("{name}-root"), None, None)];
		let mut by_ns: BTreeMap<String, usize> = BTreeMap::new();
		let mut of = BTreeMap::new();
		let mut strip = BTreeSet::new();
		for (id, def) in &types.0 {
			if def.namespaced == 0 {
				continue;
			}
			let top = &modules.path(*id)[0];
			let k = *by_ns.entry(top.ident.clone()).or_insert_with(|| {
				let package = format!("{name}-{}", top.stem.replace('_', "-"));
				let namespace = def.name.segments()[0].as_str().to_owned();
				crates.push(Crate::new(
					package,
					Some(top.ident.clone()),
					Some(namespace),
				));
				crates.len() - 1
			});
			of.insert(*id, k);
			strip.insert(*id);
		}
		let mut packages = BTreeSet::new();
		for c in &crates {
			if !packages.insert(c.package.as_str()) {
				bail!("crate {} would be generated twice", c.package);
			}
		}

		let refs: BTreeMap<TypeId, BTreeSet<TypeId>> = types
			.0
			.keys()
			.map(|id| Ok((*id, views::references(types, reg, *id)?)))
			.collect::<Result<_>>()?;
		let mut edges: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
		for (id, k) in &of {
			for t in refs[id].iter().filter_map(|r| of.get(r)) {
				if t != k {
					edges.entry(*k).or_default().insert(*t);
				}
			}
		}
		let pending: BTreeSet<TypeId> = types
			.0
			.keys()
			.copied()
			.filter(|id| !of.contains_key(id))
			.collect();
		for group in components(&pending, &refs) {
			let members: BTreeSet<TypeId> = group.iter().copied().collect();
			let wanted: BTreeSet<usize> = group
				.iter()
				.flat_map(|id| &refs[id])
				.filter(|r| !members.contains(r))
				.map(|r| of[r])
				.filter(|k| *k != ROOT)
				.collect();
			let home = if wanted.is_empty() {
				ROOT
			} else {
				match wanted
					.iter()
					.find(|c| wanted.iter().all(|w| w == *c || reaches(&edges, **c, *w)))
				{
					Some(c) => *c,
					None => bail!(
						"{} uses crates {}, none of which depends on all the others",
						types[group[0]].name.readable_name(),
						wanted
							.iter()
							.map(|k| crates[*k].package.as_str())
							.collect::<Vec<_>>()
							.join(", ")
					),
				}
			};
			for w in &wanted {
				if *w != home {
					edges.entry(home).or_default().insert(*w);
				}
			}
			for id in group {
				of.insert(id, home);
			}
		}
		Ok(Self {
			umbrella: name.replace('-', "_"),
			crates,
			of,
			strip,
		})
	}

	fn description(&self, split: &Split, c: &Crate) -> String {
		match &c.namespace {
			Some(ns) => split
				.namespace_docs
				.get(ns)
				.cloned()
				.unwrap_or_else(|| format!("The {ns} namespace of the {} crates", split.name)),
			None => format!("Definitions shared by the other {} crates", split.name),
		}
	}

	pub fn finish(
		&self,
		split: &Split,
		deps: &BTreeMap<usize, BTreeSet<usize>>,
		output: &mut GeneratorOutput,
	) -> Result<()> {
		if let Some((a, b)) = cycle(deps) {
			bail!(
				"crates {} and {} depend on each other",
				self.crates[a].package,
				self.crates[b].package
			);
		}
		let used: Vec<usize> = (0..self.crates.len())
			.filter(|k| output.contains_key(&self.crates[*k].file("lib.rs")))
			.collect();
		let viewed: Vec<usize> = used
			.iter()
			.copied()
			.filter(|k| output.contains_key(&self.crates[*k].file("view/mod.rs")))
			.collect();
		for k in &used {
			let c = &self.crates[*k];
			let lib = output
				.get_mut(&c.file("lib.rs"))
				.expect("used crates have a root");
			let view = if viewed.contains(k) {
				"pub mod view;\n"
			} else {
				""
			};
			*lib = format!("{}\n{view}{lib}", split.prelude.trim_end());
			let siblings = deps
				.get(k)
				.into_iter()
				.flatten()
				.map(|d| self.crates[*d].package.as_str());
			output.insert(
				PathBuf::from(format!("{}/Cargo.toml", c.package)),
				manifest(
					&c.package,
					split.version(&c.package),
					&self.description(split, c),
					siblings.chain(split.dependencies.iter().map(String::as_str)),
				),
			);
		}

		let mut lib = String::new();
		for k in &used {
			let c = &self.crates[*k];
			match &c.ns {
				None => lib.push_str(&format!("pub use ::{}::*;\n", c.ident)),
				Some(ns) => lib.push_str(&format!("pub use ::{} as {ns};\n", c.ident)),
			}
		}
		if !viewed.is_empty() {
			lib.push_str("pub mod view {\n");
			for k in &viewed {
				let c = &self.crates[*k];
				match &c.ns {
					None => lib.push_str(&format!("pub use ::{}::view::*;\n", c.ident)),
					Some(ns) => lib.push_str(&format!("pub use ::{}::view as {ns};\n", c.ident)),
				}
			}
			lib.push_str("}\n");
		}
		output.insert(PathBuf::from(format!("{}/src/lib.rs", split.name)), lib);
		let siblings = used.iter().map(|k| self.crates[*k].package.as_str());
		output.insert(
			PathBuf::from(format!("{}/Cargo.toml", split.name)),
			manifest(
				&split.name,
				split.version(&split.name),
				&split
					.root_doc
					.clone()
					.unwrap_or_else(|| format!("Every {} crate under one name", split.name)),
				siblings,
			),
		);
		Ok(())
	}
}

fn manifest<'a>(
	package: &str,
	version: &str,
	description: &str,
	dependencies: impl IntoIterator<Item = &'a str>,
) -> String {
	let mut deps: Vec<String> = dependencies
		.into_iter()
		.map(|d| format!("{d}.workspace = true"))
		.collect();
	deps.sort();
	format!(
		"[package]\nname = \"{package}\"\ndescription = \"{}\"\nversion = \"{version}\"\nedition.workspace = true\nlicense.workspace = true\n\n[dependencies]\n{}",
		escape(description.lines().next().unwrap_or_default().trim()),
		deps.iter().map(|d| format!("{d}\n")).collect::<String>()
	)
}

fn escape(text: &str) -> String {
	text.chars()
		.flat_map(|c| match c {
			'"' | '\\' => vec!['\\', c],
			c if c.is_control() => vec![],
			c => vec![c],
		})
		.collect()
}

fn reaches(edges: &BTreeMap<usize, BTreeSet<usize>>, from: usize, to: usize) -> bool {
	let mut seen = BTreeSet::new();
	let mut queue = vec![from];
	while let Some(k) = queue.pop() {
		if k == to {
			return true;
		}
		if seen.insert(k) {
			queue.extend(edges.get(&k).into_iter().flatten());
		}
	}
	false
}

fn cycle(edges: &BTreeMap<usize, BTreeSet<usize>>) -> Option<(usize, usize)> {
	edges
		.iter()
		.flat_map(|(a, bs)| bs.iter().map(move |b| (*a, *b)))
		.find(|(a, b)| reaches(edges, *b, *a))
}

struct Tarjan<'a> {
	inside: &'a BTreeSet<TypeId>,
	refs: &'a BTreeMap<TypeId, BTreeSet<TypeId>>,
	index: BTreeMap<TypeId, usize>,
	low: BTreeMap<TypeId, usize>,
	stack: Vec<TypeId>,
	on_stack: BTreeSet<TypeId>,
	out: Vec<Vec<TypeId>>,
}

impl Tarjan<'_> {
	fn visit(&mut self, v: TypeId) {
		let i = self.index.len();
		self.index.insert(v, i);
		self.low.insert(v, i);
		self.stack.push(v);
		self.on_stack.insert(v);
		for w in self.refs[&v]
			.iter()
			.copied()
			.filter(|w| self.inside.contains(w))
			.collect::<Vec<_>>()
		{
			let reached = match self.index.get(&w).copied() {
				None => {
					self.visit(w);
					self.low[&w]
				}
				Some(iw) if self.on_stack.contains(&w) => iw,
				Some(_) => continue,
			};
			let low = self.low.get_mut(&v).expect("visited");
			*low = (*low).min(reached);
		}
		if self.low[&v] == self.index[&v] {
			let mut group = Vec::new();
			loop {
				let w = self.stack.pop().expect("v is on the stack");
				self.on_stack.remove(&w);
				group.push(w);
				if w == v {
					break;
				}
			}
			self.out.push(group);
		}
	}
}

fn components(
	inside: &BTreeSet<TypeId>,
	refs: &BTreeMap<TypeId, BTreeSet<TypeId>>,
) -> Vec<Vec<TypeId>> {
	let mut t = Tarjan {
		inside,
		refs,
		index: BTreeMap::new(),
		low: BTreeMap::new(),
		stack: Vec::new(),
		on_stack: BTreeSet::new(),
		out: Vec::new(),
	};
	for v in inside {
		if !t.index.contains_key(v) {
			t.visit(*v);
		}
	}
	t.out
}
