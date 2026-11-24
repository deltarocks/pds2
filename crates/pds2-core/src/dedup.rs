use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use anyhow::{Context as _, Result, bail};

use crate::ast::{Block, Ident, Item, ItemArgs, ParseValue, Path, Statement, Value};

pub const ATTRIBUTE: &str = "deduplicate";

pub fn canonical(v: &Value) -> Value {
	match v {
		Value::Item(i) => {
			let mut args = ItemArgs::builder();
			for a in i.args.positional() {
				args = args.arg(canonical(a));
			}
			for (n, a) in i.args.named_values() {
				args = args.named(n.clone(), canonical(a));
			}
			Value::Item(Item {
				name: i.name.clone(),
				args: args.build(),
				block: Block {
					attributes: i.block.attributes.clone(),
					statements: i.block.statements.iter().map(canonical_statement).collect(),
					root: i.block.root,
				},
			})
		}
		Value::List(xs) => Value::List(xs.iter().map(canonical).collect()),
		Value::Record(fs) => {
			Value::Record(fs.iter().map(|(n, x)| (n.clone(), canonical(x))).collect())
		}
		Value::Binary(op, a, b) => {
			Value::Binary(*op, Box::new(canonical(a)), Box::new(canonical(b)))
		}
		v => v.clone(),
	}
}

pub fn canonical_statement(stmt: &Statement) -> Statement {
	Statement {
		attributes: stmt.attributes.clone(),
		items: stmt.items.iter().map(canonical).collect(),
	}
}

pub fn statement_text(stmt: &Statement) -> String {
	let block = Block {
		attributes: Default::default(),
		statements: vec![canonical_statement(stmt)],
		root: true,
	};
	crate::ast::format(&block)
		.split_whitespace()
		.collect::<Vec<_>>()
		.join(" ")
}

fn item_paths(v: &Value, out: &mut Vec<Path>) {
	match v {
		Value::Item(i) => {
			out.push(i.name.clone());
			for a in i.args.positional() {
				item_paths(a, out);
			}
			for (_, a) in i.args.named_values() {
				item_paths(a, out);
			}
			for s in &i.block.statements {
				for x in &s.items {
					item_paths(x, out);
				}
			}
		}
		Value::List(xs) => xs.iter().for_each(|x| item_paths(x, out)),
		Value::Record(fs) => fs.iter().for_each(|(_, x)| item_paths(x, out)),
		Value::Binary(_, a, b) => {
			item_paths(a, out);
			item_paths(b, out);
		}
		_ => {}
	}
}

pub fn definition_paths(stmt: &Statement) -> Vec<Path> {
	let mut paths = Vec::new();
	for v in stmt.items.iter().skip(1) {
		item_paths(v, &mut paths);
	}
	if let Some(head) = stmt.head() {
		for s in &head.block.statements {
			for v in &s.items {
				item_paths(v, &mut paths);
			}
		}
	}
	paths
}

pub fn resolve_lexically(
	names: &BTreeSet<String>,
	ns: &[String],
	path: &Path,
	not: &str,
) -> Option<String> {
	let tail: Vec<&str> = path
		.path
		.iter()
		.chain([&path.item])
		.map(Ident::as_str)
		.collect();
	let prefixes = if path.absolute { 0..=0 } else { 0..=ns.len() };
	prefixes.rev().find_map(|k| {
		let candidate = ns[..k]
			.iter()
			.map(String::as_str)
			.chain(tail.iter().copied())
			.collect::<Vec<_>>()
			.join("::");
		(candidate != not && names.contains(&candidate)).then_some(candidate)
	})
}

pub fn deep_hashes(graph: &BTreeMap<&str, (&str, Vec<&str>)>) -> BTreeMap<String, u64> {
	struct Tarjan<'g, 'a> {
		graph: &'g BTreeMap<&'a str, (&'a str, Vec<&'a str>)>,
		index: BTreeMap<&'a str, usize>,
		low: BTreeMap<&'a str, usize>,
		stack: Vec<&'a str>,
		on_stack: BTreeSet<&'a str>,
		components: Vec<Vec<&'a str>>,
	}
	impl<'a> Tarjan<'_, 'a> {
		fn visit(&mut self, v: &'a str) {
			let i = self.index.len();
			self.index.insert(v, i);
			self.low.insert(v, i);
			self.stack.push(v);
			self.on_stack.insert(v);
			for &w in &self.graph[v].1 {
				let reached = match self.index.get(w).copied() {
					None => {
						self.visit(w);
						self.low[w]
					}
					Some(iw) if self.on_stack.contains(w) => iw,
					Some(_) => continue,
				};
				let low = self.low.get_mut(v).expect("visited");
				*low = (*low).min(reached);
			}
			if self.low[v] == self.index[v] {
				let mut group = Vec::new();
				loop {
					let w = self.stack.pop().expect("v is on the stack");
					self.on_stack.remove(w);
					group.push(w);
					if w == v {
						break;
					}
				}
				group.sort_unstable();
				self.components.push(group);
			}
		}
	}
	let mut t = Tarjan {
		graph,
		index: BTreeMap::new(),
		low: BTreeMap::new(),
		stack: Vec::new(),
		on_stack: BTreeSet::new(),
		components: Vec::new(),
	};
	for &v in graph.keys() {
		if !t.index.contains_key(v) {
			t.visit(v);
		}
	}
	let mut component_of: BTreeMap<&str, usize> = BTreeMap::new();
	let mut component_hash: Vec<u64> = Vec::new();
	let mut out = BTreeMap::new();
	for (k, group) in t.components.iter().enumerate() {
		for &m in group {
			component_of.insert(m, k);
		}
		let mut h = DefaultHasher::new();
		for &m in group {
			let (text, refs) = &graph[m];
			m.hash(&mut h);
			text.hash(&mut h);
			for &r in refs {
				match component_of[r] {
					c if c == k => r.hash(&mut h),
					c => component_hash[c].hash(&mut h),
				}
			}
		}
		let shared = h.finish();
		component_hash.push(shared);
		for &m in group {
			let mut h = DefaultHasher::new();
			shared.hash(&mut h);
			m.hash(&mut h);
			out.insert(m.to_owned(), h.finish());
		}
	}
	out
}

struct Def {
	ns: Vec<String>,
	item: String,
	native: bool,
	statement: Statement,
	text: String,
	refs: Vec<String>,
}

fn first_name(item: &Item) -> Option<String> {
	match item.args.positional().first()? {
		Value::Item(i) => Some(i.name.item.as_str().to_owned()),
		_ => None,
	}
}

fn collect(block: &Block, ns: &mut Vec<String>, out: &mut BTreeMap<String, Def>) {
	for stmt in &block.statements {
		let Some(head) = stmt.head() else { continue };
		let Some(name) = first_name(head) else {
			continue;
		};
		match head.head_name() {
			Some("namespace") => {
				ns.push(name);
				collect(&head.block, ns, out);
				ns.pop();
			}
			Some(kind @ ("def" | "def_native")) => {
				let full = ns
					.iter()
					.chain([&name])
					.cloned()
					.collect::<Vec<_>>()
					.join("::");
				out.insert(
					full,
					Def {
						ns: ns.clone(),
						item: name,
						native: kind == "def_native",
						statement: stmt.clone(),
						text: statement_text(stmt),
						refs: vec![],
					},
				);
			}
			_ => {}
		}
	}
}

fn package_defs(block: &Block) -> BTreeMap<String, Def> {
	let mut defs = BTreeMap::new();
	collect(block, &mut Vec::new(), &mut defs);
	let names: BTreeSet<String> = defs.keys().cloned().collect();
	for (full, def) in defs.iter_mut() {
		let refs: BTreeSet<String> = definition_paths(&def.statement)
			.iter()
			.filter_map(|p| resolve_lexically(&names, &def.ns, p, full))
			.collect();
		def.refs = refs.into_iter().collect();
	}
	defs
}

fn hashes(defs: &BTreeMap<String, Def>) -> BTreeMap<String, u64> {
	let graph = defs
		.iter()
		.map(|(n, d)| {
			(
				n.as_str(),
				(d.text.as_str(), d.refs.iter().map(String::as_str).collect()),
			)
		})
		.collect();
	deep_hashes(&graph)
}

fn rewrite_value(v: &mut Value, map: &dyn Fn(&Path) -> Option<Path>, head: bool) {
	match v {
		Value::Item(item) => {
			if head {
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
			rewrite_references(&mut item.block, map);
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

fn rewrite_references(block: &mut Block, map: &dyn Fn(&Path) -> Option<Path>) {
	for stmt in &mut block.statements {
		for (i, v) in stmt.items.iter_mut().enumerate() {
			rewrite_value(v, map, i == 0);
		}
	}
}

fn rewrite_definition(stmt: &mut Statement, map: &dyn Fn(&Path) -> Option<Path>) {
	for (i, v) in stmt.items.iter_mut().enumerate() {
		match (i, v) {
			(0, Value::Item(head)) => rewrite_references(&mut head.block, map),
			(0, _) => {}
			(_, v) => rewrite_value(v, map, false),
		}
	}
}

fn alias(item: &str, target: Path) -> Statement {
	let def = Item::simple_builder("def")
		.argsb(ItemArgs::builder().arg(Value::new_simple_item(item)))
		.block(Block {
			statements: vec![Statement::simple(vec![Value::new_simple_item("export")])],
			..Default::default()
		})
		.build();
	Statement::simple(vec![
		Value::Item(def),
		Value::Item(Item::path_builder(target).build()),
	])
}

fn namespace_block<'b>(block: &'b mut Block, name: &str) -> &'b mut Block {
	let found = block.statements.iter().position(|s| {
		s.head().is_some_and(|h| {
			h.head_name() == Some("namespace") && first_name(h).as_deref() == Some(name)
		})
	});
	let index = match found {
		Some(i) => i,
		None => {
			let ns = Item::simple_builder("namespace")
				.argsb(ItemArgs::builder().arg(Value::new_simple_item(name)))
				.build();
			block
				.statements
				.push(Statement::simple(vec![Value::Item(ns)]));
			block.statements.len() - 1
		}
	};
	match &mut block.statements[index].items[0] {
		Value::Item(item) => &mut item.block,
		_ => unreachable!("namespace statements are items"),
	}
}

fn rewrite(block: &mut Block, ns: &mut Vec<String>, replace: &BTreeMap<String, Option<Statement>>) {
	for mut stmt in std::mem::take(&mut block.statements) {
		let Some(head) = stmt.head() else {
			block.statements.push(stmt);
			continue;
		};
		let Some(name) = first_name(head) else {
			block.statements.push(stmt);
			continue;
		};
		let full = ns
			.iter()
			.chain([&name])
			.cloned()
			.collect::<Vec<_>>()
			.join("::");
		match head.head_name() {
			Some("namespace") => {
				ns.push(name);
				if let Some(Value::Item(item)) = stmt.items.first_mut() {
					rewrite(&mut item.block, ns, replace);
				}
				ns.pop();
				block.statements.push(stmt);
			}
			Some("def" | "def_native") => {
				if let Some(Some(replacement)) = replace.get(&full) {
					block.statements.push(replacement.clone());
				}
			}
			_ => block.statements.push(stmt),
		}
	}
}

pub fn deduplicate(mut block: Block) -> Result<Block> {
	let Some(flag) = block.attributes.remove(&Ident::new(ATTRIBUTE)) else {
		return Ok(block);
	};
	let enabled = match flag.as_slice() {
		[v] => bool::parse(v.clone()).with_context(|| format!("@!{ATTRIBUTE}"))?,
		_ => bail!("@!{ATTRIBUTE} takes true or false"),
	};
	if !enabled {
		return Ok(block);
	}
	let packages: Vec<(usize, String)> = block
		.statements
		.iter()
		.enumerate()
		.filter_map(|(i, s)| {
			let head = s.head()?;
			(head.head_name() == Some("package")).then(|| Some((i, first_name(head)?)))?
		})
		.collect();
	let defs: Vec<BTreeMap<String, Def>> = packages
		.iter()
		.map(|(i, _)| package_defs(&block.statements[*i].head().expect("package").block))
		.collect();
	let hashes: Vec<BTreeMap<String, u64>> = defs.iter().map(hashes).collect();

	let mut natives: BTreeMap<String, (usize, u64)> = BTreeMap::new();
	let mut homes: BTreeMap<(String, u64), usize> = BTreeMap::new();
	for (p, package_defs) in defs.iter().enumerate() {
		for (full, def) in package_defs {
			if !def.native {
				homes.entry((full.clone(), hashes[p][full])).or_insert(p);
			}
		}
	}
	let names: Vec<BTreeSet<String>> = defs.iter().map(|d| d.keys().cloned().collect()).collect();
	let home_path = |p: usize, full: &str| -> Path {
		let mut segments: Vec<Ident> = full.split("::").map(Ident::new).collect();
		let item = segments.pop().expect("a definition has a name");
		if !defs[p][full].native {
			let home = homes[&(full.to_owned(), hashes[p][full])];
			segments.insert(0, Ident::new(&packages[home].1));
		}
		Path {
			absolute: true,
			path: segments,
			item,
		}
	};
	let resolved = |p: usize, def: &Def| -> Statement {
		let mut stmt = def.statement.clone();
		let map = |path: &Path| {
			resolve_lexically(&names[p], &def.ns, path, "").map(|full| home_path(p, &full))
		};
		rewrite_definition(&mut stmt, &map);
		stmt
	};
	let mut replace: Vec<BTreeMap<String, Option<Statement>>> =
		vec![BTreeMap::new(); packages.len()];
	let mut hoisted: Vec<(Vec<String>, Statement)> = Vec::new();
	for (p, package_defs) in defs.iter().enumerate() {
		for (full, def) in package_defs {
			let h = hashes[p][full];
			if def.native {
				match natives.get(full) {
					Some((first, other)) if *other != h => bail!(
						"native {full} differs between packages {} and {}",
						packages[*first].1,
						packages[p].1
					),
					Some(_) => {}
					None => {
						natives.insert(full.clone(), (p, h));
						hoisted.push((def.ns.clone(), resolved(p, def)));
					}
				}
				replace[p].insert(full.clone(), None);
				continue;
			}
			let home = homes[&(full.clone(), h)];
			if home == p {
				replace[p].insert(full.clone(), Some(resolved(p, def)));
			} else {
				let target = Path {
					absolute: true,
					path: std::iter::once(packages[home].1.as_str())
						.chain(def.ns.iter().map(String::as_str))
						.map(Ident::new)
						.collect(),
					item: Ident::new(&def.item),
				};
				replace[p].insert(full.clone(), Some(alias(&def.item, target)));
			}
		}
	}
	for ((i, _), replace) in packages.iter().zip(&replace) {
		if let Some(Value::Item(item)) = block.statements[*i].items.first_mut() {
			rewrite(&mut item.block, &mut Vec::new(), replace);
		}
	}
	let mut root = Block::default();
	for (ns, statement) in hoisted {
		let mut at = &mut root;
		for n in &ns {
			at = namespace_block(at, n);
		}
		at.statements.push(statement);
	}
	root.statements.append(&mut block.statements);
	block.statements = root.statements;
	Ok(block)
}

#[cfg(test)]
mod tests {
	use super::deduplicate;
	use crate::ast::{format, parse};

	const NATIVES: &str = "@scalar u8 @encoding be def_native(u8);\n@scalar u16 @encoding be def_native(u16);\n@specializer container def_native(container) { body { item(field) { argument(name); type(Value); }; }; };\n";

	fn aliased(out: &str, home: &str, item: &str) -> bool {
		out.contains(&format!(
			"def({item}) {{\n\t\texport;\n\t}}\n\t=> root::{home}::{item};"
		))
	}

	fn package(name: &str, leaf: &str) -> String {
		format!(
			"package({name}) {{\n{NATIVES}def(a) => container {{ field(x) => {leaf}; }};\ndef(b) => container {{ field(y) => a; }};\ndef(c) => u8;\n}};\n"
		)
	}

	#[test]
	fn later_packages_alias_identical_definitions() {
		let schema = format!(
			"@!deduplicate true\n{}{}",
			package("p1", "u8"),
			package("p2", "u8")
		);
		let out = format(&deduplicate(parse(&schema).unwrap()).unwrap());
		assert!(
			aliased(&out, "p1", "a") && aliased(&out, "p1", "b") && aliased(&out, "p1", "c"),
			"{out}"
		);
		assert!(out.contains("field(y) => root::p1::a;"), "{out}");
		assert_eq!(out.matches("def_native(u8)").count(), 1, "{out}");
		assert!(
			out.find("def_native(u8)") < out.find("package(p1)"),
			"{out}"
		);
		assert!(!out.contains("deduplicate"), "{out}");
	}

	#[test]
	fn changes_propagate_through_references() {
		let schema = format!(
			"@!deduplicate true\n{}{}",
			package("p1", "u8"),
			package("p2", "u16")
		);
		let out = format(&deduplicate(parse(&schema).unwrap()).unwrap());
		assert!(
			!aliased(&out, "p1", "a") && !aliased(&out, "p1", "b"),
			"{out}"
		);
		assert!(aliased(&out, "p1", "c"), "{out}");
		assert!(out.contains("field(y) => root::p2::a;"), "{out}");
	}

	#[test]
	fn cycles_change_as_a_whole() {
		let cyclic = |name: &str, leaf: &str| {
			format!(
				"package({name}) {{\n{NATIVES}def(d) => container {{ field(e) => s; }};\ndef(i) => container {{ field(d) => d; }};\ndef(s) => container {{ field(leaf) => {leaf}; field(i) => i; }};\n}};\n"
			)
		};
		let schema = format!(
			"@!deduplicate true\n{}{}",
			cyclic("p1", "u8"),
			cyclic("p2", "u16")
		);
		let out = format(&deduplicate(parse(&schema).unwrap()).unwrap());
		assert!(!out.contains("export;"), "{out}");
	}

	#[test]
	fn differing_natives_are_rejected() {
		let schema = "@!deduplicate true\npackage(p1) { @scalar u8 def_native(n); };\npackage(p2) { @scalar u16 def_native(n); };\n";
		let err = format!("{:#}", deduplicate(parse(schema).unwrap()).unwrap_err());
		assert!(
			err.contains("native n differs between packages p1 and p2"),
			"{err}"
		);
	}

	#[test]
	fn without_the_attribute_nothing_changes() {
		let schema = format!("{}{}", package("p1", "u8"), package("p2", "u8"));
		let parsed = parse(&schema).unwrap();
		assert_eq!(
			format(&deduplicate(parsed.clone()).unwrap()),
			format(&parsed)
		);
	}
}
