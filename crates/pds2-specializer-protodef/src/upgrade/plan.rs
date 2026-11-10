use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

pub fn head(t: &Value) -> Option<&str> {
	t.as_array()?.first()?.as_str()
}

pub fn arg(t: &Value) -> Option<&Value> {
	t.as_array()?.get(1)
}

fn is_anon(field: &Value) -> bool {
	field
		.get("anon")
		.is_some_and(|v| !matches!(v, Value::Bool(false) | Value::Null))
}

// https://github.com/ProtoDef-io/node-protodef/blob/master/src/datatypes/compiler-structures.js#L171
pub fn inline_fields(fields: &[Value]) -> Vec<(String, Value)> {
	let named = |f: &Value| {
		let name = f
			.get("name")
			.and_then(Value::as_str)
			.unwrap_or("")
			.to_owned();
		(name, f.clone())
	};
	let mut out: Vec<(String, Value)> = Vec::new();
	let mut anon = 0;
	for f in fields {
		if !is_anon(f) {
			out.push(named(f));
			continue;
		}
		let ty = f.get("type");
		if ty.and_then(head) == Some("container") {
			for inner in ty
				.and_then(arg)
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
			{
				out.push(named(inner));
			}
			continue;
		}
		let name = if anon == 0 {
			"ANON".to_owned()
		} else {
			format!("ANON_{anon}")
		};
		anon += 1;
		out.push((name, f.clone()));
	}
	out
}

// https://github.com/ProtoDef-io/node-protodef/blob/master/src/datatypes/compiler-structures.js#L38
pub fn scope_names(fields: &[(String, Value)]) -> BTreeSet<String> {
	let mut out = BTreeSet::new();
	for (name, f) in fields {
		out.insert(name.clone());
		let ty = f.get("type");
		if is_anon(f) && ty.and_then(head) == Some("bitfield") {
			for sub in ty
				.and_then(arg)
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
			{
				if let Some(n) = sub.get("name").and_then(Value::as_str) {
					out.insert(n.to_owned());
				}
			}
		}
	}
	out
}

pub fn inlined_names(fields: &[(String, Value)]) -> BTreeMap<String, String> {
	let mut out = BTreeMap::new();
	for (name, f) in fields {
		let ty = f.get("type");
		if is_anon(f) && ty.and_then(head) == Some("bitfield") {
			for sub in ty
				.and_then(arg)
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
			{
				if let Some(n) = sub.get("name").and_then(Value::as_str) {
					out.insert(n.to_owned(), name.clone());
				}
			}
		}
	}
	out
}

pub fn inlined_types(fields: &[(String, Value)]) -> BTreeMap<String, Value> {
	let mut out = BTreeMap::new();
	for (_, f) in fields {
		let ty = f.get("type");
		if is_anon(f) && ty.and_then(head) == Some("bitfield") {
			for sub in ty
				.and_then(arg)
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
			{
				let (Some(n), Some(size)) = (
					sub.get("name").and_then(Value::as_str),
					sub.get("size").and_then(Value::as_u64),
				) else {
					continue;
				};
				let signed = sub.get("signed").and_then(Value::as_bool).unwrap_or(false);
				let bits = match size {
					0..=8 => 8,
					9..=16 => 16,
					17..=32 => 32,
					_ => 64,
				};
				let prim = format!("{}{bits}", if signed { "i" } else { "u" });
				out.insert(n.to_owned(), Value::String(prim));
			}
		}
	}
	out
}

pub fn ref_head(r: &str) -> &str {
	r.trim_start_matches("../").split('/').next().unwrap_or("")
}

// https://github.com/ProtoDef-io/node-protodef/blob/master/src/compiler.js#L198
pub fn resolve(r: &str, frames: &[BTreeSet<String>]) -> Option<usize> {
	let mut rest = r;
	let mut ups = 0;
	while let Some(s) = rest.strip_prefix("../") {
		ups += 1;
		rest = s;
	}
	let head = rest.split('/').next()?;
	if head.starts_with('$') {
		return None;
	}
	let start = frames.len().checked_sub(1 + ups)?;
	(0..=start).rev().find(|&i| frames[i].contains(head))
}

pub struct RefWalker<'f> {
	on_ref: &'f mut dyn FnMut(&str, &[BTreeSet<String>]),
}

impl<'f> RefWalker<'f> {
	pub fn new(on_ref: &'f mut dyn FnMut(&str, &[BTreeSet<String>])) -> Self {
		Self { on_ref }
	}

	pub fn container(&mut self, fields: &[(String, Value)], frames: &mut Vec<BTreeSet<String>>) {
		for (_, f) in fields {
			if let Some(t) = f.get("type") {
				self.walk_type(t, frames);
			}
		}
	}

	fn on_ref(&mut self, r: &str, frames: &[BTreeSet<String>]) {
		(self.on_ref)(r, frames);
	}

	pub fn walk_type(&mut self, t: &Value, frames: &mut Vec<BTreeSet<String>>) {
		let Some(a) = t.as_array() else {
			return;
		};
		match a.first().and_then(Value::as_str) {
			Some("container") => {
				let fields = inline_fields(a.get(1).and_then(Value::as_array).map_or(&[], |v| v));
				frames.push(scope_names(&fields));
				self.container(&fields, frames);
				frames.pop();
			}
			Some("switch") => {
				let Some(sw) = a.get(1) else { return };
				if let Some(c) = sw.get("compareTo").and_then(Value::as_str) {
					self.on_ref(c, frames);
				}
				for v in sw
					.get("fields")
					.and_then(Value::as_object)
					.into_iter()
					.flat_map(|m| m.values())
				{
					self.walk_type(v, frames);
				}
				if let Some(d) = sw.get("default") {
					self.walk_type(d, frames);
				}
			}
			Some("mapper") => {}
			Some(_) => {
				for x in &a[1..] {
					self.walk_arg(x, frames);
				}
			}
			None => {}
		}
	}

	fn walk_arg(&mut self, x: &Value, frames: &mut Vec<BTreeSet<String>>) {
		match x {
			Value::Array(_) => self.walk_type(x, frames),
			Value::Object(m) => {
				for (k, v) in m {
					if k == "type" || k.ends_with("Type") {
						self.walk_type(v, frames);
					} else if k == "name" || k.ends_with("Val") {
						continue;
					} else if let Some(s) = v.as_str() {
						self.on_ref(s, frames);
					} else {
						self.walk_arg(v, frames);
					}
				}
			}
			_ => {}
		}
	}
}

pub fn count_refs(fields: &[(String, Value)], target: &str) -> usize {
	let mut count = 0;
	let mut on_ref = |r: &str, frames: &[BTreeSet<String>]| {
		if ref_head(r) == target && resolve(r, frames) == Some(0) {
			count += 1;
		}
	};
	RefWalker::new(&mut on_ref).container(fields, &mut vec![scope_names(fields)]);
	count
}

fn escapes(t: &Value, scope: &BTreeSet<String>) -> bool {
	let mut escaping = false;
	let mut on_ref = |r: &str, frames: &[BTreeSet<String>]| {
		let ups = r.matches("../").count();
		escaping |= ups > 0 && ups + 1 >= frames.len();
	};
	RefWalker::new(&mut on_ref).walk_type(t, &mut vec![scope.clone()]);
	escaping
}

pub fn product(
	tag: &str,
	switches: &[&Value],
	scope: &BTreeSet<String>,
) -> Option<Map<String, Value>> {
	let mut columns = Vec::new();
	let mut keys = BTreeSet::new();
	for field in switches {
		let sw = arg(field.get("type")?)?.as_object()?;
		let arms = sw.get("fields")?.as_object()?;
		if arms
			.values()
			.chain(sw.get("default"))
			.any(|p| escapes(p, scope))
		{
			return None;
		}
		keys.extend(arms.keys().cloned());
		let mut meta = field.as_object()?.clone();
		meta.remove("type");
		columns.push((meta, arms, sw.get("default")));
	}
	let void = json!("void");
	let arm = |pick: &dyn Fn(&Map<String, Value>, Option<&Value>) -> Value| -> Value {
		let fields: Vec<Value> = columns
			.iter()
			.filter_map(|(meta, arms, default)| {
				let ty = pick(arms, *default);
				(ty != void).then(|| {
					let mut m = meta.clone();
					m.insert("type".to_owned(), ty);
					Value::Object(m)
				})
			})
			.collect();
		if fields.is_empty() {
			void.clone()
		} else {
			json!(["container", fields])
		}
	};
	let mut fields = Map::new();
	for k in &keys {
		fields.insert(
			k.clone(),
			arm(&|arms, default| {
				arms.get(k)
					.or(default)
					.cloned()
					.unwrap_or_else(|| void.clone())
			}),
		);
	}
	let mut out = Map::new();
	out.insert("compareTo".to_owned(), json!(tag));
	out.insert("fields".to_owned(), Value::Object(fields));
	let default = arm(&|_, default| default.cloned().unwrap_or_else(|| void.clone()));
	if default != void {
		out.insert("default".to_owned(), default);
	}
	Some(out)
}

enum Wrap {
	Option,
	Container(Map<String, Value>),
}

pub fn fuse(
	switch: &Map<String, Value>,
	scope: &BTreeSet<String>,
	target: &str,
) -> Option<Map<String, Value>> {
	let mut frames = vec![scope.clone()];
	let mut wraps = Vec::new();
	let mut cur = switch.get("default")?.clone();
	let inner = loop {
		match head(&cur)? {
			"option" => {
				wraps.push(Wrap::Option);
				cur = arg(&cur)?.clone();
			}
			"container" => {
				let fields = inline_fields(arg(&cur)?.as_array()?);
				let [(_, only)] = fields.as_slice() else {
					return None;
				};
				frames.push(scope_names(&fields));
				let mut meta = only.as_object()?.clone();
				let ty = meta.remove("type")?;
				wraps.push(Wrap::Container(meta));
				cur = ty;
			}
			"switch" => break arg(&cur)?.as_object()?.clone(),
			_ => return None,
		}
	};
	let compare = inner.get("compareTo")?.as_str()?;
	if ref_head(compare) != target || resolve(compare, &frames) != Some(0) {
		return None;
	}
	let wrap = |v: Value| {
		wraps.iter().rev().fold(v, |acc, w| match w {
			Wrap::Option => json!(["option", acc]),
			Wrap::Container(meta) => {
				let mut m = meta.clone();
				m.insert("type".to_owned(), acc);
				json!(["container", [m]])
			}
		})
	};
	let mut fields = switch.get("fields")?.as_object()?.clone();
	for (k, v) in inner.get("fields")?.as_object()? {
		if !fields.contains_key(k) {
			fields.insert(k.clone(), wrap(v.clone()));
		}
	}
	let mut out = switch.clone();
	out.insert("fields".to_owned(), Value::Object(fields));
	out.insert(
		"default".to_owned(),
		wrap(
			inner
				.get("default")
				.cloned()
				.unwrap_or_else(|| json!("void")),
		),
	);
	Some(out)
}
