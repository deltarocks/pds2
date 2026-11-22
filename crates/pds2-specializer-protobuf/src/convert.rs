use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;

use anyhow::{Context as _, Result, anyhow, bail};
use protobuf_descriptor::{
	DescriptorProto, Edition, EnumDescriptorProto, FeatureSet, FeatureSetFieldPresence,
	FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet, Label, Type,
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
		item(implicit) { argument(number); type(Value); };
		item(optional) { argument(number); type(Value); };
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
	presence: Presence,
}

#[derive(Clone, Copy)]
enum Presence {
	Explicit,
	Implicit,
	Required,
}

fn overridden(base: Presence, features: Option<&FeatureSet>) -> Result<Presence> {
	Ok(match features.and_then(|f| f.field_presence.as_ref()) {
		None | Some(FeatureSetFieldPresence::FieldPresenceUnknown) => base,
		Some(FeatureSetFieldPresence::Explicit) => Presence::Explicit,
		Some(FeatureSetFieldPresence::Implicit) => Presence::Implicit,
		Some(FeatureSetFieldPresence::LegacyRequired) => Presence::Required,
		Some(FeatureSetFieldPresence::Unknown(n)) => bail!("unknown field presence {n}"),
	})
}

fn file_presence(file: &FileDescriptorProto) -> Result<Presence> {
	let base = match file.syntax.as_deref() {
		None | Some("" | "proto2") => Presence::Explicit,
		Some("proto3") => Presence::Implicit,
		Some("editions") => match &file.edition {
			Some(Edition::EditionProto3) => Presence::Implicit,
			Some(
				Edition::EditionProto2
				| Edition::Edition2023
				| Edition::Edition2024
				| Edition::Edition2026,
			) => Presence::Explicit,
			e => bail!("edition {e:?} is not supported"),
		},
		Some(s) => bail!("unknown syntax {s:?}"),
	};
	overridden(
		base,
		file.options.as_ref().and_then(|o| o.features.as_ref()),
	)
}

fn name(n: &Option<String>) -> Result<&str> {
	n.as_deref()
		.ok_or_else(|| anyhow!("descriptor without a name"))
}

fn collect_message<'a>(
	out: &mut BTreeMap<String, Decl<'a>>,
	package: &[String],
	scope: &str,
	prefix: &str,
	msg: &'a DescriptorProto,
	inherited: Presence,
) -> Result<()> {
	let n = name(&msg.name)?;
	let full = format!("{scope}.{n}");
	let local = if prefix.is_empty() {
		n.to_owned()
	} else {
		format!("{prefix}_{n}")
	};
	let presence = overridden(
		inherited,
		msg.options.as_ref().and_then(|o| o.features.as_ref()),
	)?;
	for nested in &msg.nested_type {
		collect_message(out, package, &full, &local, nested, presence)?;
	}
	for e in &msg.enum_type {
		let en = name(&e.name)?;
		out.insert(
			format!("{full}.{en}"),
			Decl {
				package: package.to_vec(),
				local: format!("{local}_{en}"),
				kind: Kind::Enum(e),
				presence,
			},
		);
	}
	out.insert(
		full,
		Decl {
			package: package.to_vec(),
			local,
			kind: Kind::Message(msg),
			presence,
		},
	);
	Ok(())
}

fn scalar(ty: &Type) -> Result<Option<&'static str>> {
	Ok(Some(match ty {
		Type::TypeDouble => "double",
		Type::TypeFloat => "float",
		Type::TypeInt64 => "int64",
		Type::TypeUint64 => "uint64",
		Type::TypeInt32 => "int32",
		Type::TypeFixed64 => "fixed64",
		Type::TypeFixed32 => "fixed32",
		Type::TypeBool => "bool",
		Type::TypeString => "string",
		Type::TypeBytes => "bytes",
		Type::TypeUint32 => "uint32",
		Type::TypeSfixed32 => "sfixed32",
		Type::TypeSfixed64 => "sfixed64",
		Type::TypeSint32 => "sint32",
		Type::TypeSint64 => "sint64",
		Type::TypeMessage | Type::TypeEnum => return Ok(None),
		Type::TypeGroup => bail!("proto2 groups are not supported"),
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
	let ty = f
		.r#type
		.as_ref()
		.ok_or_else(|| anyhow!("field without a type"))?;
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

type Extensions<'a> = BTreeMap<String, Vec<(String, &'a FieldDescriptorProto)>>;

fn collect_extensions<'a>(
	out: &mut Extensions<'a>,
	scope: &str,
	fields: &'a [FieldDescriptorProto],
) -> Result<()> {
	for f in fields {
		let extendee = f
			.extendee
			.as_deref()
			.ok_or_else(|| anyhow!("extension {:?} without an extendee", f.name))?;
		out.entry(extendee.to_owned())
			.or_default()
			.push((format!("{scope}.{}", name(&f.name)?), f));
	}
	Ok(())
}

fn collect_message_extensions<'a>(
	out: &mut Extensions<'a>,
	scope: &str,
	msg: &'a DescriptorProto,
) -> Result<()> {
	let full = format!("{scope}.{}", name(&msg.name)?);
	collect_extensions(out, &full, &msg.extension)?;
	for nested in &msg.nested_type {
		collect_message_extensions(out, &full, nested)?;
	}
	Ok(())
}

fn message(
	out: &mut String,
	decls: &BTreeMap<String, Decl>,
	local: &str,
	msg: &DescriptorProto,
	presence: Presence,
	extensions: &[(String, &FieldDescriptorProto)],
) -> Result<()> {
	write!(out, "def({local}) => message {{")?;
	let mut taken = BTreeSet::new();
	for f in &msg.field {
		let fname = name(&f.name)?;
		taken.insert(fname.to_owned());
		let oneof = match (f.oneof_index, f.proto3_optional) {
			(Some(i), None | Some(false)) => Some(
				msg.oneof_decl
					.get(usize::try_from(i)?)
					.and_then(|o| o.name.as_deref())
					.ok_or_else(|| anyhow!("field {fname} has a dangling oneof index {i}"))?,
			),
			_ => None,
		};
		let explicit = f.proto3_optional == Some(true)
			|| matches!(f.r#type, Some(Type::TypeMessage | Type::TypeGroup));
		let item = match oneof {
			Some(g) => Item::Oneof(g),
			None if matches!(f.label, Some(Label::LabelRequired)) => Item::Required,
			None => match overridden(
				presence,
				f.options.as_ref().and_then(|o| o.features.as_ref()),
			)? {
				Presence::Required => Item::Required,
				Presence::Implicit if !explicit => Item::Implicit,
				Presence::Implicit | Presence::Explicit => Item::Optional,
			},
		};
		message_field(out, decls, fname, f, item)?;
	}
	for (full, f) in extensions {
		let mut fname = full.trim_start_matches('.').replace('.', "_");
		while !taken.insert(fname.clone()) {
			fname.push('_');
		}
		message_field(out, decls, &fname, f, Item::Optional)
			.with_context(|| format!("extension {full}"))?;
	}
	writeln!(out, "}};")?;
	Ok(())
}

enum Item<'a> {
	Required,
	Implicit,
	Optional,
	Oneof(&'a str),
}

fn message_field(
	out: &mut String,
	decls: &BTreeMap<String, Decl>,
	fname: &str,
	f: &FieldDescriptorProto,
	item: Item,
) -> Result<()> {
	let number = f
		.number
		.ok_or_else(|| anyhow!("field {fname} without a number"))?;
	let (ty, _) = field_type(decls, f).with_context(|| format!("field {fname}"))?;
	if matches!(f.label, Some(Label::LabelRepeated)) {
		write!(out, "repeated({fname}, number: {number}) => {ty};")?;
		return Ok(());
	}
	match item {
		Item::Required => write!(out, "field({fname}, number: {number}) => {ty};")?,
		Item::Implicit => write!(out, "implicit({fname}, number: {number}) => {ty};")?,
		Item::Optional => write!(out, "optional({fname}, number: {number}) => {ty};")?,
		Item::Oneof(g) => write!(
			out,
			"oneof({fname}, number: {number}, group: \"{g}\") => {ty};"
		)?,
	}
	Ok(())
}

fn enumeration(out: &mut String, local: &str, e: &EnumDescriptorProto) -> Result<()> {
	write!(out, "def({local}) => enum(Tag: root::int32) {{")?;
	for v in &e.value {
		let vname = name(&v.name)?;
		let number = v
			.number
			.ok_or_else(|| anyhow!("enum value {vname} without a number"))?;
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
	let mut extensions = Extensions::new();
	let mut queue = VecDeque::new();
	for file in &set.file {
		let package: Vec<String> = file
			.package
			.as_deref()
			.filter(|p| !p.is_empty())
			.map(|p| p.split('.').map(str::to_owned).collect())
			.unwrap_or_default();
		let scope = package
			.iter()
			.fold(String::new(), |s, p| format!("{s}.{p}"));
		let is_root = roots.is_empty() || roots.iter().any(|r| file.name.as_deref() == Some(r));
		let before: BTreeSet<String> = decls.keys().cloned().collect();
		let presence = file_presence(file).with_context(|| format!("in {:?}", file.name))?;
		collect_extensions(&mut extensions, &scope, &file.extension)?;
		for msg in &file.message_type {
			collect_message(&mut decls, &package, &scope, "", msg, presence)?;
			collect_message_extensions(&mut extensions, &scope, msg)?;
		}
		for e in &file.enum_type {
			let en = name(&e.name)?;
			decls.insert(
				format!("{scope}.{en}"),
				Decl {
					package: package.clone(),
					local: en.to_owned(),
					kind: Kind::Enum(e),
					presence,
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
			let extended = extensions.get(&full).into_iter().flatten().map(|(_, f)| *f);
			for f in m.field.iter().chain(extended) {
				if let (_, Some(dep)) =
					field_type(&decls, f).with_context(|| format!("in {full}"))?
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
			Kind::Message(m) => message(
				&mut def,
				&decls,
				&decl.local,
				m,
				decl.presence,
				extensions.get(full).map_or(&[], Vec::as_slice),
			),
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
	let parsed =
		pds2_core::ast::parse(&out).map_err(|e| anyhow!("generated pds does not parse: {e}"))?;
	Ok(pds2_core::ast::format(&parsed))
}
