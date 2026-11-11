use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context as _, Result};
use heck::{ToSnakeCase, ToUpperCamelCase};

use crate::ast::Ident;
use crate::hir::TypeId;
use crate::mir::TypeMap;

#[derive(Clone, Copy, Debug)]
pub enum Case {
	Snake,
	Pascal,
}

const KEYWORDS: &[&str] = &[
	"as", "async", "await", "box", "break", "const", "continue", "do", "dyn", "else", "enum",
	"extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
	"mut", "pub", "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use",
	"where", "while", "yield", "abstract", "become", "final", "macro", "override", "priv", "try",
	"typeof", "unsized", "virtual", "gen",
];
const UNRAWABLE: &[&str] = &["self", "Self", "super", "crate"];

pub fn escape(name: &str) -> String {
	if KEYWORDS.contains(&name) {
		format!("r#{name}")
	} else if UNRAWABLE.contains(&name) {
		format!("{name}_")
	} else {
		name.to_owned()
	}
}

fn convert(case: Case, name: &str) -> Option<String> {
	let out = match case {
		Case::Snake => name.to_snake_case(),
		Case::Pascal => name.to_upper_camel_case(),
	};
	let valid = out
		.chars()
		.next()
		.is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
		&& out.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
	valid.then_some(out)
}

#[derive(Clone, Debug)]
pub struct Scope {
	case: Case,
	names: BTreeMap<String, String>,
}

impl Scope {
	pub fn new<'a>(
		case: Case,
		names: impl IntoIterator<Item = &'a str>,
		reserved: &[&str],
	) -> Result<Self> {
		let names: BTreeSet<&str> = names.into_iter().collect();
		let mut converts_to: BTreeMap<String, usize> = BTreeMap::new();
		for n in &names {
			if let Some(c) = convert(case, n) {
				*converts_to.entry(c).or_default() += 1;
			}
		}
		let mut taken: BTreeMap<String, &str> = BTreeMap::new();
		let mut out = BTreeMap::new();
		for n in &names {
			let chosen = match convert(case, n) {
				Some(c) if converts_to[&c] == 1 && !reserved.contains(&c.as_str()) => c,
				_ => n.to_string(),
			};
			if reserved.contains(&chosen.as_str()) {
				bail!("{n:?} clashes with the generated item {chosen:?}");
			}
			let chosen = escape(&chosen);
			if let Some(other) = taken.insert(chosen.clone(), n) {
				bail!("{other:?} and {n:?} both generate {chosen:?}");
			}
			out.insert(n.to_string(), chosen);
		}
		Ok(Self { case, names: out })
	}

	pub fn empty(case: Case) -> Self {
		Self {
			case,
			names: BTreeMap::new(),
		}
	}

	pub fn get(&self, raw: &str) -> String {
		match self.names.get(raw) {
			Some(n) => n.clone(),
			None => escape(&convert(self.case, raw).unwrap_or_else(|| raw.to_owned())),
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Module {
	pub ident: String,
	pub stem: String,
}

impl Module {
	pub fn new(ident: String) -> Self {
		let plain = ident.strip_prefix("r#").unwrap_or(&ident);
		let stem = if plain == "mod" {
			"mod_".to_owned()
		} else {
			plain.to_owned()
		};
		Self { ident, stem }
	}
	pub fn needs_path(&self) -> bool {
		self.ident.strip_prefix("r#").unwrap_or(&self.ident) != self.stem
	}
}

pub struct Modules {
	paths: BTreeMap<TypeId, Vec<Module>>,
}

pub type Dirs = BTreeMap<Vec<Module>, BTreeMap<Module, bool>>;

impl Modules {
	pub fn new(
		types: &TypeMap,
		name: impl Fn(&BTreeSet<Ident>) -> Result<BTreeMap<Ident, Module>>,
	) -> Result<Self> {
		let mut dirs: BTreeMap<Vec<Ident>, BTreeSet<Ident>> = BTreeMap::new();
		for def in types.0.values() {
			let full = def.name.segments();
			for i in 0..full.len() {
				dirs.entry(full[..i].to_vec())
					.or_default()
					.insert(full[i].clone());
			}
		}
		let mut named = BTreeMap::new();
		for (dir, children) in &dirs {
			let names = name(children).with_context(|| {
				let path: Vec<&str> = dir.iter().map(Ident::as_str).collect();
				format!("naming modules in namespace {:?}", path.join("::"))
			})?;
			named.insert(dir.clone(), names);
		}
		let paths = types
			.0
			.iter()
			.map(|(id, def)| {
				let full = def.name.segments();
				let path = (0..full.len())
					.map(|i| named[&full[..i]][&full[i]].clone())
					.collect();
				(*id, path)
			})
			.collect();
		Ok(Self { paths })
	}

	pub fn path(&self, id: TypeId) -> &[Module] {
		&self.paths[&id]
	}

	pub fn idents(&self, id: TypeId) -> impl Iterator<Item = &str> {
		self.path(id).iter().map(|m| m.ident.as_str())
	}

	pub fn file(&self, id: TypeId, ext: &str) -> String {
		let stems: Vec<&str> = self.path(id).iter().map(|m| m.stem.as_str()).collect();
		format!("{}.{ext}", stems.join("/"))
	}

	pub fn dirs(&self, ids: impl IntoIterator<Item = TypeId>) -> Dirs {
		let mut out: Dirs = BTreeMap::new();
		out.entry(vec![]).or_default();
		for id in ids {
			let path = self.path(id);
			for i in 0..path.len() {
				let is_dir = i + 1 < path.len();
				out.entry(path[..i].to_vec())
					.or_default()
					.insert(path[i].clone(), is_dir);
			}
		}
		out
	}
}

pub fn rust_modules(types: &TypeMap) -> Result<Modules> {
	Modules::new(types, |children| {
		let scope = Scope::new(Case::Snake, children.iter().map(Ident::as_str), &[])?;
		Ok(children
			.iter()
			.map(|c| (c.clone(), Module::new(scope.get(c.as_str()))))
			.collect())
	})
}

pub fn raw_modules(types: &TypeMap) -> Result<Modules> {
	Modules::new(types, |children| {
		Ok(children
			.iter()
			.map(|c| (c.clone(), Module::new(c.as_str().to_owned())))
			.collect())
	})
}

pub fn relative<'a>(from: &[&'a str], to: &[&'a str]) -> (usize, Vec<&'a str>) {
	let common = from.iter().zip(to).take_while(|(a, b)| a == b).count();
	(from.len() - common, to[common..].to_vec())
}

pub fn rust_relative(from: &[&str], to: &[&str]) -> String {
	let (up, rest) = relative(from, to);
	let head = if up == 0 {
		vec!["self"]
	} else {
		vec!["super"; up]
	};
	head.into_iter().chain(rest).collect::<Vec<_>>().join("::")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn conventional_names_unless_they_clash() {
		let s = Scope::new(Case::Snake, ["fooBar", "foo_bar", "Baz", "type", "self"], &[]).unwrap();
		assert_eq!(s.get("fooBar"), "fooBar");
		assert_eq!(s.get("foo_bar"), "foo_bar");
		assert_eq!(s.get("Baz"), "baz");
		assert_eq!(s.get("type"), "r#type");
		assert_eq!(s.get("self"), "self_");

		let p = Scope::new(Case::Pascal, ["LABEL_OPTIONAL", "number_value", "self", "ty"], &["Ty"]).unwrap();
		assert_eq!(p.get("LABEL_OPTIONAL"), "LabelOptional");
		assert_eq!(p.get("number_value"), "NumberValue");
		assert_eq!(p.get("self"), "Self_");
		assert_eq!(p.get("ty"), "ty");
	}

	#[test]
	fn unresolvable_clashes_are_errors() {
		assert!(Scope::new(Case::Pascal, ["Ty"], &["Ty"]).is_err());
		assert!(Scope::new(Case::Snake, ["self", "self_"], &[]).is_err());
	}

	#[test]
	fn relative_paths() {
		assert_eq!(rust_relative(&["a", "b", "T"], &["a", "b", "U"]), "super::U");
		assert_eq!(rust_relative(&["a", "b", "T"], &["a", "c", "U"]), "super::super::c::U");
		assert_eq!(rust_relative(&["view", "a", "T"], &["a", "T"]), "super::super::super::a::T");
		assert_eq!(rust_relative(&["view"], &["view", "a", "T"]), "self::a::T");
		assert_eq!(rust_relative(&["T"], &["T"]), "self");
	}
}
