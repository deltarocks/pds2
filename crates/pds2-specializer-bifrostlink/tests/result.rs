use std::path::PathBuf;

use pds2_core::generator::rust;
use pds2_core::specializer::Registry;
use pds2_core::{ast, hir, mir};

const SCHEMA: &str = r#"
@scalar u8 @encoding be
def_native(u8);
@scalar u16 @encoding be
def_native(u16);
@specializer container
def_native(container) {
	body {
		item(field) { type(Value); };
		item(virtual_field) { argument(value); type(Value); };
	};
};
@specializer result
def_native(result) {
	type(Ok);
	type(Err);
};
def(Holder) => container {
	field(outcome) => result(Ok: u16, Err: u8);
};
"#;

#[test]
fn result_is_a_std_result() {
	let mut reg = Registry::default();
	pds2_specializer_bytes::register(&mut reg);
	pds2_specializer_protodef::register(&mut reg);
	pds2_specializer_bifrostlink::register(&mut reg);
	let types = mir::from_hir(
		&hir::parse_ast(ast::parse(SCHEMA).unwrap(), &reg).unwrap(),
		&reg,
	)
	.unwrap();
	let mut out = Default::default();
	rust::generate(&types, &mut out, &reg, None, None, &[]).unwrap();
	let holder = &out[&PathBuf::from("holder.rs")];
	assert!(holder.contains("::core::result::Result<"), "{holder}");
	assert!(holder.contains("Clone"), "{holder}");
	let result = out
		.iter()
		.find(|(path, _)| path.to_string_lossy().starts_with("result"))
		.map(|(_, v)| v)
		.expect("result module");
	assert!(result.contains("InvalidValue"), "{result}");
}
