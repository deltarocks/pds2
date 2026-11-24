use std::path::PathBuf;

use pds2_core::generator::rust;
use pds2_core::specializer::Registry;
use pds2_core::{ast, hir, mir};

const SCHEMA: &str = r#"
@scalar u8 @encoding be
def_native(u8);
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
def_native(void) {
	is_void;
};
@specializer pipe
def_native(pipe);
def(Holder) => container {
	field(io) => pipe;
	field(tag) => u8;
};
def(Stdio) => union(Tag: u8) {
	variant(null, match: 0);
	variant(pipe, match: 1) => pipe;
};
def(Spec) => container {
	field(stdin) => Stdio;
	field(tag) => u8;
};
def(Plain) => container {
	field(tag) => u8;
};
"#;

fn types(reg: &Registry) -> mir::TypeMap {
	mir::from_hir(
		&hir::parse_ast(ast::parse(SCHEMA).unwrap(), reg).unwrap(),
		reg,
	)
	.unwrap()
}

fn registry() -> Registry {
	let mut reg = Registry::default();
	pds2_specializer_bytes::register(&mut reg);
	pds2_specializer_protodef::register(&mut reg);
	pds2_specializer_bifrostlink::register(&mut reg);
	reg
}

fn generated() -> std::collections::HashMap<PathBuf, String> {
	let reg = registry();
	let mut out = Default::default();
	rust::generate(&types(&reg), &mut out, &reg, None, None, &[]).unwrap();
	out
}

fn move_only(file: &str) {
	assert!(file.contains("#[derive(Debug)]"), "{file}");
	assert!(!file.contains("Clone"), "{file}");
	assert!(!file.contains("PartialEq"), "{file}");
}

#[test]
fn holders_are_move_only() {
	let out = generated();
	let holder = &out[&PathBuf::from("holder.rs")];
	assert!(holder.contains("pub io: ::bifrostlink::Pipe"), "{holder}");
	move_only(holder);
	let pipe = &out[&PathBuf::from("pipe.rs")];
	assert!(
		pipe.contains("::bifrostlink::__private::pipe::serialize"),
		"{pipe}"
	);
}

#[test]
fn unions_inside_containers_are_move_only() {
	let out = generated();
	move_only(&out[&PathBuf::from("stdio.rs")]);
	move_only(&out[&PathBuf::from("spec.rs")]);
	let plain = &out[&PathBuf::from("plain.rs")];
	assert!(plain.contains("Clone"), "{plain}");
}

#[test]
fn views_reject_pipes() {
	let reg = registry();
	let mut out = Default::default();
	let e = rust::generate(&types(&reg), &mut out, &reg, Some(""), None, &[]).unwrap_err();
	assert!(format!("{e:#}").contains("move-only"), "{e:#}");
}

#[test]
fn typescript_pipes_go_through_the_runtime() {
	let reg = registry();
	let mut out = Default::default();
	pds2_core::generator::typescript::generator(&types(&reg), &mut out, &reg).unwrap();
	let holder = &out[&PathBuf::from("Holder.ts")];
	assert!(holder.contains("io: Pipe;"), "{holder}");
	let stdio = &out[&PathBuf::from("Stdio.ts")];
	assert!(stdio.contains("{ kind: \"pipe\"; value: Pipe }"), "{stdio}");
	let pipe = &out[&PathBuf::from("pipe.ts")];
	for expected in [
		"import {Input, Output, Pipe, pipe} from \"../pds.ts\";",
		"export type Ty = Pipe;",
		"pipe.serialize(_0, output);",
		"return pipe.deserialize(input);",
	] {
		assert!(pipe.contains(expected), "{expected:?} missing from {pipe}");
	}
}
