use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use pds2_core::ast::{self, Block, Item, ItemArgs, Statement, Value};

pub struct Version {
	pub name: String,
	pub defs: BTreeMap<String, Def>,
}

pub struct Def {
	pub native: bool,
	pub text: String,
	pub refs: BTreeSet<String>,
	pub shape: Shape,
}

pub enum Shape {
	Container(Vec<(String, String)>),
	Union {
		tag: String,
		arms: Vec<(String, String, String)>,
	},
	Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
	Renumbered,
	FieldsAdded,
	FieldsRemoved,
	FieldsRenamed,
	FieldsRetypedScalar,
	FieldsRetypedNested,
	FieldsReordered,
	ArmsAdded,
	ArmsRemoved,
	ArmsChanged,
	TagChanged,
	ShapeChanged,
	Other,
}

fn canonical(v: &Value) -> Value {
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
		Value::Record(fs) => Value::Record(fs.iter().map(|(n, x)| (n.clone(), canonical(x))).collect()),
		Value::Binary(op, a, b) => Value::Binary(*op, Box::new(canonical(a)), Box::new(canonical(b))),
		v => v.clone(),
	}
}

fn canonical_statement(stmt: &Statement) -> Statement {
	Statement {
		attributes: stmt.attributes.clone(),
		items: stmt.items.iter().map(canonical).collect(),
	}
}

fn format_values(items: &[Value]) -> String {
	if items.is_empty() {
		return String::new();
	}
	let block = Block {
		attributes: Default::default(),
		statements: vec![Statement::simple(items.iter().map(canonical).collect())],
		root: true,
	};
	let text = ast::format(&block);
	text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_statement(stmt: &Statement) -> String {
	let block = Block {
		attributes: Default::default(),
		statements: vec![canonical_statement(stmt)],
		root: true,
	};
	ast::format(&block)
		.split_whitespace()
		.collect::<Vec<_>>()
		.join(" ")
}

fn first_name(item: &Item) -> Option<String> {
	match item.args.positional().first()? {
		Value::Item(i) => Some(i.name.item.as_str().to_owned()),
		_ => None,
	}
}

fn item_paths(v: &Value, out: &mut Vec<ast::Path>) {
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

fn shape(ty: Option<&Value>) -> Shape {
	let Some(Value::Item(item)) = ty else {
		return Shape::Other;
	};
	match item.name.item.as_str() {
		"container" => Shape::Container(
			item.block
				.statements
				.iter()
				.filter_map(|s| {
					let Value::Item(head) = s.items.first()? else {
						return None;
					};
					let name = first_name(head)?;
					let full = format_values(&s.items);
					Some((name.clone(), full.replacen(&format!("({name}"), "(", 1)))
				})
				.collect(),
		),
		"union" => Shape::Union {
			tag: format_values(&[Value::Item(Item {
				name: item.name.clone(),
				args: item.args.clone(),
				block: Block::default(),
			})]),
			arms: item
				.block
				.statements
				.iter()
				.filter_map(|s| {
					let Value::Item(head) = s.items.first()? else {
						return None;
					};
					let name =
						first_name(head).unwrap_or_else(|| head.name.item.as_str().to_owned());
					let matched = head
						.args
						.named_values()
						.find(|(n, _)| n.as_str() == "match")
						.map(|(_, v)| format_values(std::slice::from_ref(v)))
						.unwrap_or_default();
					Some((name, matched, format_values(&s.items[1..])))
				})
				.collect(),
		},
		_ => Shape::Other,
	}
}

struct Raw {
	native: bool,
	text: String,
	paths: Vec<ast::Path>,
	ns: Vec<String>,
	shape: Shape,
}

fn collect(block: &Block, ns: &mut Vec<String>, out: &mut BTreeMap<String, Raw>) {
	for stmt in &block.statements {
		let Some(Value::Item(head)) = stmt.items.first() else {
			continue;
		};
		match head.name.item.as_str() {
			"namespace" => {
				let Some(name) = first_name(head) else {
					continue;
				};
				ns.push(name);
				collect(&head.block, ns, out);
				ns.pop();
			}
			kind @ ("def" | "def_native") => {
				let Some(name) = first_name(head) else {
					continue;
				};
				let mut paths = Vec::new();
				for v in &stmt.items[1..] {
					item_paths(v, &mut paths);
				}
				for s in &head.block.statements {
					for v in &s.items {
						item_paths(v, &mut paths);
					}
				}
				let full = ns
					.iter()
					.chain(std::iter::once(&name))
					.cloned()
					.collect::<Vec<_>>()
					.join("::");
				out.insert(
					full,
					Raw {
						native: kind == "def_native",
						text: format_statement(stmt),
						paths,
						ns: ns.clone(),
						shape: shape(stmt.items.get(1)),
					},
				);
			}
			_ => {}
		}
	}
}

pub fn load(name: &str, block: &Block) -> Version {
	let mut raw = BTreeMap::new();
	collect(block, &mut Vec::new(), &mut raw);
	let names: BTreeSet<String> = raw.keys().cloned().collect();
	let defs = raw
		.into_iter()
		.map(|(full, r)| {
			let mut refs = BTreeSet::new();
			for p in &r.paths {
				let tail: Vec<String> = p
					.path
					.iter()
					.chain(std::iter::once(&p.item))
					.map(|i| i.as_str().to_owned())
					.collect();
				let prefixes = if p.absolute { 0..=0 } else { 0..=r.ns.len() };
				for k in prefixes.rev() {
					let candidate = r.ns[..k]
						.iter()
						.chain(&tail)
						.cloned()
						.collect::<Vec<_>>()
						.join("::");
					if candidate != full && names.contains(&candidate) {
						refs.insert(candidate);
						break;
					}
				}
			}
			(
				full,
				Def {
					native: r.native,
					text: r.text,
					refs,
					shape: r.shape,
				},
			)
		})
		.collect();
	Version {
		name: name.to_owned(),
		defs,
	}
}

fn hash(s: &str) -> u64 {
	let mut h = DefaultHasher::new();
	s.hash(&mut h);
	h.finish()
}

pub fn deep(v: &Version) -> BTreeMap<String, u64> {
	fn go(
		v: &Version,
		name: &str,
		memo: &mut BTreeMap<String, u64>,
		active: &mut BTreeSet<String>,
	) -> u64 {
		if let Some(h) = memo.get(name) {
			return *h;
		}
		let def = &v.defs[name];
		if !active.insert(name.to_owned()) {
			return hash(&def.text);
		}
		let mut h = DefaultHasher::new();
		def.text.hash(&mut h);
		for r in &def.refs {
			go(v, r, memo, active).hash(&mut h);
		}
		active.remove(name);
		let out = h.finish();
		memo.insert(name.to_owned(), out);
		out
	}
	let mut memo = BTreeMap::new();
	for name in v.defs.keys() {
		go(v, name, &mut memo, &mut BTreeSet::new());
	}
	memo
}

fn simple(ty: &str) -> bool {
	let ty = ty.split_once("=> ").map_or(ty, |(_, t)| t);
	!ty.contains('{') && !ty.contains('(') && !ty.contains("=>")
}

pub fn classify(a: &Def, b: &Def) -> (BTreeSet<Change>, Vec<String>) {
	let mut out = BTreeSet::new();
	let mut detail = Vec::new();
	match (&a.shape, &b.shape) {
		(Shape::Container(old), Shape::Container(new)) => {
			let old_names: Vec<&String> = old.iter().map(|(n, _)| n).collect();
			let new_names: Vec<&String> = new.iter().map(|(n, _)| n).collect();
			let ty = |list: &[(String, String)], n: &String| {
				list.iter()
					.find(|(m, _)| m == n)
					.map(|(_, t)| t.clone())
					.unwrap_or_default()
			};
			let mut added: Vec<&String> = new_names
				.iter()
				.filter(|n| !old_names.contains(n))
				.copied()
				.collect();
			let mut removed: Vec<&String> = old_names
				.iter()
				.filter(|n| !new_names.contains(n))
				.copied()
				.collect();
			removed.retain(|r| {
				let t = ty(old, r);
				match added.iter().position(|a| ty(new, a) == t) {
					Some(i) => {
						detail.push(format!("{r} renamed to {}", added[i]));
						added.remove(i);
						out.insert(Change::FieldsRenamed);
						false
					}
					None => true,
				}
			});
			for n in &added {
				out.insert(Change::FieldsAdded);
				detail.push(format!("+{n}: {}", ty(new, n)));
			}
			for n in &removed {
				out.insert(Change::FieldsRemoved);
				detail.push(format!("-{n}: {}", ty(old, n)));
			}
			let common_old: Vec<&String> = old_names
				.iter()
				.filter(|n| new_names.contains(n))
				.copied()
				.collect();
			let common_new: Vec<&String> = new_names
				.iter()
				.filter(|n| old_names.contains(n))
				.copied()
				.collect();
			if common_old != common_new {
				out.insert(Change::FieldsReordered);
				detail.push("fields reordered".to_owned());
			}
			for n in &common_old {
				let (t0, t1) = (ty(old, n), ty(new, n));
				if t0 == t1 {
					continue;
				}
				if simple(&t0) && simple(&t1) {
					out.insert(Change::FieldsRetypedScalar);
					detail.push(format!("~{n}: {t0} -> {t1}"));
				} else {
					out.insert(Change::FieldsRetypedNested);
					detail.push(format!("~{n}: nested change"));
				}
			}
		}
		(Shape::Union { tag: ta, arms: old }, Shape::Union { tag: tb, arms: new }) => {
			if ta != tb {
				out.insert(Change::TagChanged);
				detail.push(format!("tag {ta} -> {tb}"));
			}
			let find = |l: &[(String, String, String)], n: &str| {
				l.iter().find(|(m, _, _)| m == n).cloned()
			};
			let mut renumbered = 0;
			for (n, m, p) in new {
				match find(old, n) {
					None => {
						out.insert(Change::ArmsAdded);
						detail.push(format!("+{n}"));
					}
					Some((_, m0, p0)) => {
						if m0 != *m {
							renumbered += 1;
						}
						if p0 != *p {
							out.insert(Change::ArmsChanged);
							detail.push(format!("~{n}"));
						}
					}
				}
			}
			for (n, _, _) in old {
				if find(new, n).is_none() {
					out.insert(Change::ArmsRemoved);
					detail.push(format!("-{n}"));
				}
			}
			if renumbered > 0 {
				out.insert(Change::Renumbered);
				detail.push(format!("{renumbered} arms renumbered"));
			}
		}
		(Shape::Other, Shape::Other) => {
			out.insert(Change::Other);
		}
		_ => {
			out.insert(Change::ShapeChanged);
		}
	}
	if out.is_empty() {
		out.insert(Change::Other);
	}
	(out, detail)
}

#[derive(Default)]
struct Transition {
	added: usize,
	dropped: usize,
	identical: usize,
	deps_only: usize,
	own: usize,
	kinds: BTreeMap<Change, usize>,
}

pub fn diff(a: &Version, b: &Version) -> String {
	let (da, db) = (deep(a), deep(b));
	let mut out = String::new();
	for name in b.defs.keys().filter(|n| !a.defs.contains_key(*n)) {
		let _ = writeln!(out, "+ {name}");
	}
	for name in a.defs.keys().filter(|n| !b.defs.contains_key(*n)) {
		let _ = writeln!(out, "- {name}");
	}
	for (name, new) in &b.defs {
		let Some(old) = a.defs.get(name) else {
			continue;
		};
		if old.text != new.text {
			let (kinds, detail) = classify(old, new);
			let kinds: Vec<String> = kinds.iter().map(|k| format!("{k:?}")).collect();
			let _ = writeln!(out, "~ {name} [{}]", kinds.join(", "));
			for d in detail {
				let _ = writeln!(out, "    {d}");
			}
		} else if da[name] != db[name] {
			let _ = writeln!(out, "= {name} (references changed)");
		}
	}
	out
}

pub fn report(versions: &[Version]) -> String {
	let deeps: Vec<BTreeMap<String, u64>> = versions.iter().map(deep).collect();
	let mut out = String::new();
	let total: usize = versions.iter().map(|v| v.defs.len()).sum();
	let own: BTreeSet<(String, u64)> = versions
		.iter()
		.flat_map(|v| v.defs.iter().map(|(n, d)| (n.clone(), hash(&d.text))))
		.collect();
	let deep_distinct: BTreeSet<(String, u64)> = versions
		.iter()
		.zip(&deeps)
		.flat_map(|(v, d)| v.defs.keys().map(|n| (n.clone(), d[n])))
		.collect();
	let names: BTreeSet<&String> = versions.iter().flat_map(|v| v.defs.keys()).collect();
	let _ = writeln!(out, "versions: {}", versions.len());
	let _ = writeln!(out, "definitions over all versions: {total}");
	let _ = writeln!(out, "distinct names: {}", names.len());
	let natives: BTreeSet<&String> = versions
		.iter()
		.flat_map(|v| v.defs.iter().filter(|(_, d)| d.native).map(|(n, _)| n))
		.collect();
	let _ = writeln!(out, "of which natives: {}", natives.len());
	let _ = writeln!(out, "distinct by own text: {}", own.len());
	let _ = writeln!(
		out,
		"distinct by own text and referenced types: {}",
		deep_distinct.len()
	);
	let _ = writeln!(out);
	let _ = writeln!(
		out,
		"{:<26} {:>5} {:>5} {:>5} {:>5} {:>5}  own changes by kind",
		"transition", "+new", "-gone", "same", "deps", "own"
	);
	let mut totals: BTreeMap<Change, usize> = BTreeMap::new();
	let mut churn: BTreeMap<String, usize> = BTreeMap::new();
	let mut examples: BTreeMap<Change, Vec<String>> = BTreeMap::new();
	let mut sum = Transition::default();
	for (i, pair) in versions.windows(2).enumerate() {
		let (a, b) = (&pair[0], &pair[1]);
		let (da, db) = (&deeps[i], &deeps[i + 1]);
		let mut t = Transition::default();
		for (name, def) in &b.defs {
			let Some(old) = a.defs.get(name) else {
				t.added += 1;
				continue;
			};
			if old.text != def.text {
				t.own += 1;
				*churn.entry(name.clone()).or_default() += 1;
				let (kinds, detail) = classify(old, def);
				for k in kinds {
					*t.kinds.entry(k).or_default() += 1;
					let ex = examples.entry(k).or_default();
					if ex.len() < 4 {
						ex.push(format!(
							"{} -> {} {name}: {}",
							a.name,
							b.name,
							detail.join(", ")
						));
					}
				}
			} else if da[name] != db[name] {
				t.deps_only += 1;
			} else {
				t.identical += 1;
			}
		}
		t.dropped = a.defs.keys().filter(|n| !b.defs.contains_key(*n)).count();
		let kinds = t
			.kinds
			.iter()
			.map(|(k, n)| format!("{k:?} {n}"))
			.collect::<Vec<_>>()
			.join(", ");
		let _ = writeln!(
			out,
			"{:<26} {:>5} {:>5} {:>5} {:>5} {:>5}  {kinds}",
			format!("{} -> {}", a.name, b.name),
			t.added,
			t.dropped,
			t.identical,
			t.deps_only,
			t.own
		);
		for (k, n) in t.kinds {
			*totals.entry(k).or_default() += n;
		}
		sum.added += t.added;
		sum.dropped += t.dropped;
		sum.identical += t.identical;
		sum.deps_only += t.deps_only;
		sum.own += t.own;
	}
	let _ = writeln!(
		out,
		"{:<26} {:>5} {:>5} {:>5} {:>5} {:>5}",
		"all transitions", sum.added, sum.dropped, sum.identical, sum.deps_only, sum.own
	);
	let _ = writeln!(out);
	let _ = writeln!(
		out,
		"own changes by kind (a change can have several kinds):"
	);
	for (k, n) in &totals {
		let _ = writeln!(out, "  {:<16} {n}", format!("{k:?}"));
	}
	let _ = writeln!(out);
	let _ = writeln!(out, "examples:");
	for (k, ex) in &examples {
		let _ = writeln!(out, "  {k:?}");
		for e in ex {
			let e: String = e.chars().take(200).collect();
			let _ = writeln!(out, "    {e}");
		}
	}
	let _ = writeln!(out);
	let _ = writeln!(out, "most changed definitions:");
	let mut churn: Vec<(String, usize)> = churn.into_iter().collect();
	churn.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
	for (name, n) in churn.iter().take(25) {
		let _ = writeln!(out, "  {n:>3} {name}");
	}
	out
}
