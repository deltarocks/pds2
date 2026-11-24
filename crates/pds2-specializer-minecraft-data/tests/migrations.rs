use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use pds2_core::generator::rust;
use pds2_core::generator::split::Split;
use pds2_core::specializer::Registry;
use pds2_core::{ast, hir, mir};

const NATIVES: &str = r#"
@encoding be @scalar u8 def_native(u8);
@encoding be @scalar i32 def_native(i32);
@encoding be @scalar f64 def_native(f64);
@encoding varint @scalar i32 def_native(varint);
def_native(void) { is_void; };
@specializer container
def_native(container) {
	body {
		item(field) { type(Value); };
		item(virtual_field) { argument(value); type(Value); };
	};
};
@specializer union
def_native(union) {
	body {
		item(variant) { argument(match); type(Value) => void; };
		item(default) { anonymous; type(Value) => void; };
	};
	variant(external) { argument(tag); };
	variant(inline) { type(Tag); };
};
"#;

fn registry() -> Registry {
	let mut reg = Registry::default();
	pds2_specializer_bytes::register(&mut reg);
	pds2_specializer_generic::register(&mut reg);
	pds2_specializer_protodef::register(&mut reg);
	pds2_specializer_minecraft_data::register(&mut reg);
	reg
}

fn version(name: &str, protocol: i32, x: &str, attributes: &str) -> String {
	format!(
		r#"package({name}) {{
	@!protocol [{protocol}]
	{attributes}
	{NATIVES}
	def(entity) => container {{ field(id) => i32; field(x) => {x}; }};
	namespace(play) {{
		namespace(toClient) {{
			def(packet) => union(Tag: varint) {{
				variant(move, match: 0) => entity;
				variant(ping, match: 1) => u8;
			}};
		}};
	}};
}};
"#
	)
}

fn schema(rules: &str) -> String {
	schema_with(rules, ["", "", ""])
}

fn schema_with(rules: &str, [a1, a2, a3]: [&str; 3]) -> String {
	format!(
		"@!deduplicate true\n{}{}{}package(convert) {{ migrations {{ version(v1, name: \"1\"); version(v2, name: \"2\") {{ {rules} }}; version(v3, name: \"3\"); }}; }};\n",
		version("v1", 1, "i32", a1),
		version("v2", 2, "f64", a2),
		version("v3", 3, "f64", a3),
	)
}

const CONVERT_X: &str =
	"updated(entity) { converted(x, up: f64(./x) / 32.0, down: i32(./x * 32.0)); };";

fn compile(schema: &str) -> Result<BTreeMap<PathBuf, String>> {
	let reg = registry();
	let (block, uses) = pds2_core::prepare(ast::parse(schema)?, &reg)?;
	let types = mir::from_hir(&hir::parse_ast(block, &reg)?, &reg)?;
	let split = Split {
		name: "mc".to_owned(),
		prelude: String::new(),
		dependencies: vec![],
		root_doc: None,
		namespace_docs: BTreeMap::new(),
		versions: BTreeMap::new(),
		packages: ["v1", "v2", "v3", "convert"].map(str::to_owned).into(),
		modules: [("convert".to_owned(), PathBuf::from("/host/src/convert.rs"))].into(),
	};
	let mut out = Default::default();
	rust::generate(&types, &mut out, &reg, None, Some(&split), &uses)?;
	Ok(out.into_iter().collect())
}

#[test]
fn generates_steps_into_the_module_package() {
	let out = compile(&schema(CONVERT_X)).unwrap();
	let convert = &out[&PathBuf::from("/host/src/convert.rs")];
	assert!(
		convert.starts_with("#![allow(unreachable_code)]"),
		"{convert}"
	);
	assert!(
		convert.contains(r#"pub const VERSIONS: &[&str] = &["1", "2", "3"];"#),
		"{convert}"
	);
	assert!(convert.contains("(1, Version::V1)"), "{convert}");
	assert!(
		convert.contains("pub const SNAPSHOTS: &[(i32, &str, Version)] = &[];"),
		"{convert}"
	);
	assert!(
		convert.contains("::mc::v1::play::to_client::Packet"),
		"{convert}"
	);
	assert!(
		convert.contains("::mc::v2::play::to_client::Packet"),
		"{convert}"
	);
	assert!(
		convert.contains(
			"(Root::PlayToClient, Version::V2, Version::V3) => step_playtoclient_1_2(data, cx),"
		),
		"{convert}"
	);
	assert!(
		!out.contains_key(&PathBuf::from("convert/Cargo.toml")),
		"{:?}",
		out.keys()
	);
	assert!(
		out.contains_key(&PathBuf::from("mc-v1/src/lib.rs")),
		"{:?}",
		out.keys()
	);
	assert!(
		!out.contains_key(&PathBuf::from("mc-v3/src/entity.rs")),
		"v3 re-exports v2: {:?}",
		out.keys()
	);
	assert!(
		out[&PathBuf::from("mc-v3/src/lib.rs")].contains("pub use ::mc_v2::entity as entity;"),
		"{}",
		out[&PathBuf::from("mc-v3/src/lib.rs")]
	);
	let umbrella = &out[&PathBuf::from("mc/src/lib.rs")];
	assert!(!umbrella.contains("convert"), "{umbrella}");
}

#[test]
fn reports_missing_and_unused_rules() {
	let err = format!("{:#}", compile(&schema("")).unwrap_err());
	assert!(
		err.contains("1 -> 2: entity/x: i32 became f64, mention it with `converted`"),
		"{err}"
	);
	let err = format!(
		"{:#}",
		compile(&schema(
			"updated(entity) { converted(x, up: f64(./x), down: i32(./x)); removed(gone); };"
		))
		.unwrap_err()
	);
	assert!(
		err.contains("1 -> 2: rule entity/removed(gone) is not used"),
		"{err}"
	);
}

fn grid(name: &str, protocol: i32, outer: &str, inner: &str) -> String {
	format!(
		r#"package({name}) {{
	@!protocol [{protocol}]
	{NATIVES}
	@specializer array
	def_native(array) {{
		type(Value);
		variant(inline) {{ type(Count); }};
		variant(sized) {{ argument(count); virtual_field(size) => varint; }};
	}};
	namespace(play) {{
		namespace(toClient) {{
			def(packet) => union(Tag: varint) {{
				variant(grid, match: 0) => container {{
					field(w) => u8;
					field(h) => u8;
					field(cells) => array(count: ../{outer}) => array(count: ../../{inner}) => u8;
				}};
			}};
		}};
	}};
}};
"#
	)
}

fn grid_schema(rules: &str) -> String {
	format!(
		"@!deduplicate true\n{}{}package(convert) {{ migrations {{ version(v1, name: \"1\"); version(v2, name: \"2\") {{ {rules} }}; }}; }};\n",
		grid("v1", 1, "w", "h"),
		grid("v2", 2, "h", "w"),
	)
}

#[test]
fn lists_counted_differently_need_a_rule() {
	let err = format!("{:#}", compile(&grid_schema("")).unwrap_err());
	assert!(
		err.contains("1 -> 2: play::toClient::packet/grid/cells: argument count changed from w to h, it can't be converted automatically, add a rule"),
		"{err}"
	);
	compile(&grid_schema(
		"namespace(play) { namespace(toClient) { updated(packet) { updated(grid) { updated(cells, up: unsupported, down: unsupported); }; }; }; };",
	))
	.unwrap();
}

#[test]
fn every_version_needs_a_package() {
	let schema = format!(
		"{}package(convert) {{ migrations {{ version(v1, name: \"1\"); version(v9, name: \"9\"); }}; }};",
		version("v1", 1, "i32", "")
	);
	let err = format!("{:#}", compile(&schema).unwrap_err());
	assert!(err.contains("version 9: there is no package(v9)"), "{err}");
}

#[test]
fn lists_snapshots_by_protocol() {
	let out = compile(&schema_with(
		CONVERT_X,
		[
			"",
			r#"@!snapshots [(protocol: 1073741826, name: "2-rc1"), (protocol: 1073741825, name: "2-pre1")]"#,
			r#"@!snapshots [(name: "3-snapshot-1", protocol: 1073741826)]"#,
		],
	))
	.unwrap();
	let convert = &out[&PathBuf::from("/host/src/convert.rs")];
	assert!(
		convert.contains(
			r#"pub const SNAPSHOTS: &[(i32, &str, Version)] = &[(1073741825, "2-pre1", Version::V2), (1073741826, "2-rc1", Version::V2), (1073741826, "3-snapshot-1", Version::V3)];"#
		),
		"{convert}"
	);
	assert!(
		convert.contains(
			"pub fn snapshots_of(protocol: i32) -> &'static [(i32, &'static str, Version)]"
		),
		"{convert}"
	);
}

#[test]
fn rejects_bad_snapshots() {
	let err = |attributes| {
		format!(
			"{:#}",
			compile(&schema_with(CONVERT_X, attributes)).unwrap_err()
		)
	};
	let e = err(["", "@!snapshots [1073741825]", ""]);
	assert!(
		e.contains(
			r#"version 2: `@!snapshots` lists `(protocol: <number>, name: "...")`, got Int(1073741825)"#
		),
		"{e}"
	);
	let e = err(["", r#"@!snapshots [(protocol: 1073741825)]"#, ""]);
	assert!(e.contains("version 2: `@!snapshots` lists"), "{e}");
	let e = err([
		"",
		r#"@!snapshots [(protocol: 4294967296, name: "big")]"#,
		"",
	]);
	assert!(
		e.contains("version 2: protocol 4294967296 is out of range"),
		"{e}"
	);
	let e = err(["", "", r#"@!snapshots [(protocol: 1, name: "3-pre1")]"#]);
	assert!(
		e.contains("snapshot 3-pre1 of version 3: protocol 1 is a release protocol"),
		"{e}"
	);
	let e = err([
		"",
		r#"@!snapshots [(protocol: 1073741825, name: "twice")]"#,
		r#"@!snapshots [(protocol: 1073741826, name: "twice")]"#,
	]);
	assert!(e.contains("snapshot twice is listed twice"), "{e}");
}
