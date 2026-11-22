use anyhow::{Result, anyhow};

use pds2_core::mir::{self, TypeMap};

const DECL: &str = r#"
	@scalar u8 def_native(u8);
	@scalar u16 def_native(u16);
	@specializer array
	def_native(array) { type(Value); variant(sized) { argument(count); }; };
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
	def_native(void) { is_void; };
	def(Pick) { argument(k) => u8; } => union(tag: ./$k) { variant(one, match: 1) => u8; };
	def(Hand) { argument(k) => u8; } => container { field(p) => Pick(k: ../$k); };
"#;

fn compile(def: &str) -> Result<TypeMap> {
	let reg = pds2::registry();
	let ast = pds2_core::ast::parse(&format!("{DECL} {def}")).map_err(|e| anyhow!("{e}"))?;
	mir::from_hir(&pds2_core::hir::parse_ast(ast, &reg)?, &reg)
}

fn with_k(body: &str) -> Result<TypeMap> {
	compile(&format!(
		"def(T) {{ export; argument(k) => u8; }} => container {{ {body} }};"
	))
}

fn error(body: &str) -> String {
	format!("{:#}", with_k(body).expect_err("should be rejected"))
}

#[test]
fn every_nested_item_is_one_level_for_arguments() {
	with_k("field(a) => union(tag: ../$k) { variant(one, match: 1) => u8; };").unwrap();
	with_k(
		"field(a) => array(count: 2) => union(tag: ../../$k) { variant(one, match: 1) => u8; };",
	)
	.unwrap();
	with_k("field(a) => array(count: 2) => Pick(k: ../../$k);").unwrap();
	with_k(
		"field(a) => union(Tag: u8) { variant(b, match: 0) => union(tag: ../../$k) { variant(one, match: 1) => u8; }; };",
	)
	.unwrap();
	with_k(
		"field(a) => container { field(b) => container { field(c) => Pick(k: ../../../$k); }; };",
	)
	.unwrap();
	with_k("field(a) => Hand(k: ../$k);").unwrap();
	compile("def(T) { export; } => container { field(k) => u8; field(h) => Hand(k: ../k); };")
		.unwrap();
}

#[test]
fn references_may_not_leave_their_def() {
	let err = error("field(n) => u8; field(d) => array(count: ../../n) => u8;");
	assert!(
		err.contains("../../n goes up 2 levels, its item is only 1 deep"),
		"{err}"
	);
	let err = error("field(a) => union(tag: ../../$k) { variant(one, match: 1) => u8; };");
	assert!(err.contains("../../$k goes up 2 levels"), "{err}");
	let err = error("field(a) => array(count: 2) => Pick(k: ../../../$k);");
	assert!(
		err.contains("goes up 3 levels, its item is only 2 deep"),
		"{err}"
	);
}

#[test]
fn only_the_def_has_arguments() {
	let err = error("field(a) => union(tag: ./$k) { variant(one, match: 1) => u8; };");
	assert!(
		err.contains("./$k names an argument of an item 1 deep"),
		"{err}"
	);
	let err = error("field(n) => u8; field(a) => array(count: ../n) => Pick(k: ../$count);");
	assert!(
		err.contains("../$count names an argument of an item 1 deep"),
		"{err}"
	);
}

#[test]
fn reference_arguments_start_at_the_reference() {
	let err = error("field(n) => u8; field(d) => array(count: ./n) => u8;");
	assert!(
		err.contains("./n is an argument of its item and points at that item itself"),
		"{err}"
	);
	let err =
		error("field(n) => u8; field(a) => union(tag: ./n) { variant(one, match: 1) => u8; };");
	assert!(err.contains("./n is an argument of its item"), "{err}");
	let err = error("field(p) => Pick(k: ./$k);");
	assert!(
		err.contains("./$k names an argument of an item 1 deep"),
		"{err}"
	);
}
