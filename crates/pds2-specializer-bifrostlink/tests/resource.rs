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
@specializer resource
def_native(resource) {
	type(Value);
};
def(Holder) => container {
	field(handle) => resource => u8;
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

#[test]
fn holders_are_move_only() {
	let reg = registry();
	let mut out = Default::default();
	rust::generate(&types(&reg), &mut out, &reg, None, None, &[]).unwrap();
	let holder = &out[&PathBuf::from("holder.rs")];
	assert!(holder.contains("::bifrostlink::Resource<"), "{holder}");
	assert!(holder.contains("#[derive(Debug)]"), "{holder}");
	assert!(!holder.contains("Clone"), "{holder}");
}

#[test]
fn views_reject_resources() {
	let reg = registry();
	let mut out = Default::default();
	let e = rust::generate(&types(&reg), &mut out, &reg, Some(""), None, &[]).unwrap_err();
	assert!(format!("{e:#}").contains("move-only"), "{e:#}");
}

#[test]
fn typescript_resources_go_through_the_runtime() {
	let reg = registry();
	let mut out = Default::default();
	pds2_core::generator::typescript::generator(&types(&reg), &mut out, &reg).unwrap();
	let holder = &out[&PathBuf::from("Holder.ts")];
	assert!(holder.contains("handle: Resource<pds$u8.Ty>;"), "{holder}");
	let resource = out
		.iter()
		.find(|(path, _)| path.to_string_lossy().starts_with("resource"))
		.map(|(_, v)| v)
		.expect("resource module");
	for expected in [
		"import {Input, Output, Resource, resource} from \"../pds.ts\";",
		"resource.serialize(_0, output);",
		"return resource.adopt(id, payload);",
		"return resource.id_size(_0) + pds$u8.size_of(_0.value);",
	] {
		assert!(
			resource.contains(expected),
			"{expected:?} missing from {resource}"
		);
	}
}
