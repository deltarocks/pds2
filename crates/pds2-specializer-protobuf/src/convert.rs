use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;

use anyhow::{anyhow, bail, Context as _, Result};
use protobuf_descriptor::{
	DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet, Label, Type,
};

const PRELUDE: &str = r#"
@scalar f64 @protobuf i64 @encoding le def_native(double);
@scalar f32 @protobuf i32 @encoding le def_native(float);
@scalar i64 @protobuf varint @encoding varint def_native(int64);
@scalar u64 @protobuf varint @encoding varint def_native(uint64);
@scalar i32 @protobuf varint @encoding varint64 def_native(int32);
@scalar u64 @protobuf i64 @encoding le def_native(fixed64);
@scalar u32 @protobuf i32 @encoding le def_native(fixed32);
@scalar bool @protobuf varint @encoding varint def_native(bool);
@scalar string @protobuf len @encoding raw def_native(string);
@scalar bytes @protobuf len @encoding raw def_native(bytes);
@scalar u32 @protobuf varint @encoding varint def_native(uint32);
@scalar i32 @protobuf i32 @encoding le def_native(sfixed32);
@scalar i64 @protobuf i64 @encoding le def_native(sfixed64);
@scalar i32 @protobuf varint @encoding zigzag def_native(sint32);
@scalar i64 @protobuf varint @encoding zigzag def_native(sint64);
def_native(void) { is_void; };
@specializer const
def_native(const) { type(Value); argument(value) => Value; body; };
@specializer message
def_native(message) {
	body {
		item(field) { argument(number); type(Value); };
		item(repeated) { argument(number); type(Value); };
		item(oneof) { argument(number); argument(group); type(Value); };
	};
};
@specializer enum
def_native(enum) { type(Tag); body { item(value) { argument(number); }; }; };
"#;

enum Kind<'a> {
	Message(&'a DescriptorProto),
	Enum(&'a EnumDescriptorProto),
}

struct Decl<'a> {
	package: Vec<String>,
	local: String,
	kind: Kind<'a>,
}

fn name(n: &Option<String>) -> Result<&str> {
	n.as_deref().ok_or_else(|| anyhow!("descriptor without a name"))
}

fn collect_message<'a>(
	out: &mut BTreeMap<String, Decl<'a>>,
	package: &[String],
	scope: &str,
	prefix: &str,
	msg: &'a DescriptorProto,
) -> Result<()> {
	let n = name(&msg.name)?;
	let full = format!("{scope}.{n}");
	let local = if prefix.is_empty() { n.to_owned() } else { format!("{prefix}_{n}") };
	for nested in &msg.nested_type {
		collect_message(out, package, &full, &local, nested)?;
	}
	for e in &msg.enum_type {
		let en = name(&e.name)?;
		out.insert(
			format!("{full}.{en}"),
			Decl {
				package: package.to_vec(),
				local: format!("{local}_{en}"),
				kind: Kind::Enum(e),
			},
		);
	}
	out.insert(
		full,
		Decl {
			package: package.to_vec(),
			local,
			kind: Kind::Message(msg),
		},
	);
	Ok(())
}

fn scalar(ty: &Type) -> Result<Option<&'static str>> {
	Ok(Some(match ty {
		Type::TYPE_DOUBLE => "double",
		Type::TYPE_FLOAT => "float",
		Type::TYPE_INT64 => "int64",
		Type::TYPE_UINT64 => "uint64",
		Type::TYPE_INT32 => "int32",
		Type::TYPE_FIXED64 => "fixed64",
		Type::TYPE_FIXED32 => "fixed32",
		Type::TYPE_BOOL => "bool",
		Type::TYPE_STRING => "string",
		Type::TYPE_BYTES => "bytes",
		Type::TYPE_UINT32 => "uint32",
		Type::TYPE_SFIXED32 => "sfixed32",
		Type::TYPE_SFIXED64 => "sfixed64",
		Type::TYPE_SINT32 => "sint32",
		Type::TYPE_SINT64 => "sint64",
		Type::TYPE_MESSAGE | Type::TYPE_ENUM => return Ok(None),
		Type::TYPE_GROUP => bail!("proto2 groups are not supported"),
		other => bail!("unknown field type {other:?}"),
	}))
}

fn path(decl: &Decl) -> String {
	let mut out = String::from("root");
	for p in decl.package.iter().chain(std::iter::once(&decl.local)) {
		out.push_str("::");
		out.push_str(p);
	}
	out
}

fn field_type<'a>(
	decls: &'a BTreeMap<String, Decl>,
	f: &FieldDescriptorProto,
) -> Result<(String, Option<&'a str>)> {
	let ty = f.r#type.as_ref().ok_or_else(|| anyhow!("field without a type"))?;
	if let Some(s) = scalar(ty)? {
		return Ok((format!("root::{s}"), None));
	}
	let type_name = f
		.type_name
		.as_deref()
		.ok_or_else(|| anyhow!("message field without a type name"))?;
	let (full, decl) = decls
		.get_key_value(type_name)
		.ok_or_else(|| anyhow!("unknown type {type_name}"))?;
	Ok((path(decl), Some(full.as_str())))
}

fn message(
	out: &mut String,
	decls: &BTreeMap<String, Decl>,
	local: &str,
	msg: &DescriptorProto,
) -> Result<()> {
	write!(out, "def({local}) => message {{")?;
	for f in &msg.field {
		let fname = name(&f.name)?;
		let number = f.number.ok_or_else(|| anyhow!("field {fname} without a number"))?;
		let (ty, _) = field_type(decls, f).with_context(|| format!("field {fname}"))?;
		let repeated = matches!(f.label, Some(Label::LABEL_REPEATED));
		let oneof = match (f.oneof_index, f.proto3_optional) {
			(Some(i), None | Some(false)) => Some(
				msg.oneof_decl
					.get(usize::try_from(i)?)
					.and_then(|o| o.name.as_deref())
					.ok_or_else(|| anyhow!("field {fname} has a dangling oneof index {i}"))?,
			),
			_ => None,
		};
		match (repeated, oneof) {
			(true, _) => write!(out, "repeated({fname}, number: {number}) => {ty};")?,
			(false, Some(g)) => {
				write!(out, "oneof({fname}, number: {number}, group: \"{g}\") => {ty};")?
			}
			(false, None) => write!(out, "field({fname}, number: {number}) => {ty};")?,
		}
	}
	writeln!(out, "}};")?;
	Ok(())
}

fn enumeration(out: &mut String, local: &str, e: &EnumDescriptorProto) -> Result<()> {
	write!(out, "def({local}) => enum(Tag: root::int32) {{")?;
	for v in &e.value {
		let vname = name(&v.name)?;
		let number = v.number.ok_or_else(|| anyhow!("enum value {vname} without a number"))?;
		write!(out, "value({vname}, number: {number});")?;
	}
	writeln!(out, "}};")?;
	Ok(())
}

#[derive(Default)]
struct Namespace {
	defs: Vec<String>,
	inner: BTreeMap<String, Namespace>,
}

impl Namespace {
	fn emit(&self, out: &mut String) {
		for d in &self.defs {
			out.push_str(d);
		}
		for (n, ns) in &self.inner {
			let _ = writeln!(out, "namespace({n}) {{");
			ns.emit(out);
			out.push_str("};\n");
		}
	}
}

pub fn convert(set: &FileDescriptorSet, roots: &[String]) -> Result<String> {
	let mut decls = BTreeMap::new();
	let mut queue = VecDeque::new();
	for file in &set.file {
		let package: Vec<String> = file
			.package
			.as_deref()
			.filter(|p| !p.is_empty())
			.map(|p| p.split('.').map(str::to_owned).collect())
			.unwrap_or_default();
		let scope = package.iter().fold(String::new(), |s, p| format!("{s}.{p}"));
		let is_root = roots.is_empty() || roots.iter().any(|r| file.name.as_deref() == Some(r));
		let before: BTreeSet<String> = decls.keys().cloned().collect();
		for msg in &file.message_type {
			collect_message(&mut decls, &package, &scope, "", msg)?;
		}
		for e in &file.enum_type {
			let en = name(&e.name)?;
			decls.insert(
				format!("{scope}.{en}"),
				Decl {
					package: package.clone(),
					local: en.to_owned(),
					kind: Kind::Enum(e),
				},
			);
		}
		if is_root {
			queue.extend(decls.keys().filter(|k| !before.contains(*k)).cloned());
		}
	}
	if queue.is_empty() {
		bail!("no types in the root files {roots:?}");
	}

	let mut reachable = BTreeSet::new();
	while let Some(full) = queue.pop_front() {
		if !reachable.insert(full.clone()) {
			continue;
		}
		if let Some(Decl {
			kind: Kind::Message(m),
			..
		}) = decls.get(&full)
		{
			for f in &m.field {
				if let (_, Some(dep)) = field_type(&decls, f)
					.with_context(|| format!("in {full}"))?
				{
					queue.push_back(dep.to_owned());
				}
			}
		}
	}

	let mut root = Namespace::default();
	for full in &reachable {
		let decl = &decls[full];
		let mut def = String::new();
		match decl.kind {
			Kind::Message(m) => message(&mut def, &decls, &decl.local, m),
			Kind::Enum(e) => enumeration(&mut def, &decl.local, e),
		}
		.with_context(|| format!("in {full}"))?;
		let mut ns = &mut root;
		for p in &decl.package {
			ns = ns.inner.entry(p.clone()).or_default();
		}
		ns.defs.push(def);
	}

	let mut out = String::from(PRELUDE);
	root.emit(&mut out);
	let parsed = pds2_core::ast::parse(&out).map_err(|e| anyhow!("generated pds does not parse: {e}"))?;
	Ok(pds2_core::ast::format(&parsed))
}
