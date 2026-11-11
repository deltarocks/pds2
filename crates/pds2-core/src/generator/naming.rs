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

pub fn assign(entries: &[(Case, &str)], reserved: &[&str]) -> Result<Vec<String>> {
	let mut converts_to: BTreeMap<String, usize> = BTreeMap::new();
	for (case, n) in entries {
		if let Some(c) = convert(*case, n) {
			*converts_to.entry(c).or_default() += 1;
		}
	}
	let mut taken: BTreeMap<String, (Case, &str)> = BTreeMap::new();
	let mut out = Vec::new();
	for &(case, n) in entries {
		let chosen = match convert(case, n) {
			Some(c) if converts_to[&c] == 1 && !reserved.contains(&c.as_str()) => c,
			_ => n.to_string(),
		};
		if reserved.contains(&chosen.as_str()) {
			bail!("{n:?} clashes with the generated item {chosen:?}");
		}
		let chosen = escape(&chosen);
		if let Some((other_case, other)) = taken.insert(chosen.clone(), (case, n)) {
			bail!("{other:?} ({other_case:?}) and {n:?} ({case:?}) both generate {chosen:?}");
		}
		out.push(chosen);
	}
	Ok(out)
}

impl Scope {
	pub fn new<'a>(
		case: Case,
		names: impl IntoIterator<Item = &'a str>,
		reserved: &[&str],
	) -> Result<Self> {
		let names: BTreeSet<&str> = names.into_iter().collect();
		let entries: Vec<(Case, &str)> = names.iter().map(|n| (case, *n)).collect();
		let chosen = assign(&entries, reserved)?;
		Ok(Self {
			case,
			names: names.iter().map(|n| n.to_string()).zip(chosen).collect(),
		})
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
	pub ty: Option<String>,
}

impl Module {
	pub fn new(ident: String, ty: Option<String>) -> Self {
		let plain = ident.strip_prefix("r#").unwrap_or(&ident);
		let stem = if plain == "mod" {
			"mod_".to_owned()
		} else {
			plain.to_owned()
		};
		Self { ident, stem, ty }
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
		name: impl Fn(&BTreeMap<Ident, bool>) -> Result<BTreeMap<Ident, Module>>,
	) -> Result<Self> {
		let mut dirs: BTreeMap<Vec<Ident>, BTreeMap<Ident, bool>> = BTreeMap::new();
		for def in types.0.values() {
			let full = def.name.segments();
			for i in 0..full.len() {
				*dirs.entry(full[..i].to_vec())
					.or_default()
					.entry(full[i].clone())
					.or_default() |= i + 1 == full.len();
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
		let entries: Vec<(Case, &str)> = children
			.iter()
			.map(|(name, is_type)| {
				let case = if *is_type { Case::Pascal } else { Case::Snake };
				(case, name.as_str())
			})
			.collect();
		let primary = assign(&entries, &[])?;
		let mut taken: BTreeSet<String> = primary.iter().cloned().collect();
		let mut out = BTreeMap::new();
		for ((name, is_type), first) in children.iter().zip(primary) {
			if !is_type {
				out.insert(name.clone(), Module::new(first, None));
				continue;
			}
			let mut module = convert(Case::Snake, name.as_str()).unwrap_or_else(|| name.to_string());
			while taken.contains(&escape(&module)) {
				module.push('_');
			}
			let module = escape(&module);
			taken.insert(module.clone());
			out.insert(name.clone(), Module::new(module, Some(first)));
		}
		Ok(out)
	})
}

pub fn raw_modules(types: &TypeMap) -> Result<Modules> {
	Modules::new(types, |children| {
		Ok(children
			.iter()
			.map(|(name, is_type)| {
				let raw = name.as_str().to_owned();
				(name.clone(), Module::new(raw.clone(), is_type.then_some(raw)))
			})
			.collect())
	})
}

const SHADOWED: &[&str] = &[
	"Ty", "Args", "Input", "Output", "Result", "Error", "Option", "Some", "None", "Ok", "Err",
	"Vec", "String", "Box", "Default", "Into", "From", "TryFrom", "TryInto", "Clone", "Copy",
	"PartialEq", "Eq", "PartialOrd", "Ord", "Debug", "Iterator", "IntoIterator", "FromIterator",
	"DoubleEndedIterator", "ExactSizeIterator", "Extend", "ToString", "ToOwned", "AsRef", "AsMut",
	"Send", "Sync", "Sized", "Unpin", "Drop", "Fn", "FnMut", "FnOnce",
];

pub fn nominal(ty: &str) -> bool {
	ty.starts_with(|c: char| c.is_ascii_uppercase()) && !SHADOWED.contains(&ty)
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
