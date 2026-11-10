use anyhow::{anyhow, Result};

use pds2_core::mir::{self, Expr, TypeKind, TypeMap, Value};

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
		body { item(variant) { argument(match); type(Value); }; };
		variant(external) { argument(tag); };
		variant(inline) { type(Tag); };
	};
	def_native(void) { is_void; };
	@specializer const
	def_native(const) { type(Value); argument(value) => Value; body; };
	def(Pair) => container { field(a) => u8; field(b) => u16; };
	def(Mode) => union(Tag: u8) { variant(on, match: 1) => void; variant(level, match: 2) => u16; };
	def(Pick) { argument(k) => u8; } => union(tag: ./$k) { variant(one, match: 1) => u8; };
	def(AnyPick) { type(K); argument(k) => K; } => union(tag: ./$k) { variant(one, match: 1) => u8; };
	@specializer terminated
	def_native(terminated) { type(Value); type(End); argument(end) => End; body; };
"#;

fn compile(body: &str) -> Result<TypeMap> {
	let reg = pds2::registry();
	let src = format!("{DECL} def(T) {{ export; }} => container {{ {body} }};");
	let ast = pds2_core::ast::parse(&src).map_err(|e| anyhow!("{e}"))?;
	mir::from_hir(&pds2_core::hir::parse_ast(ast, &reg)?, &reg)
}

fn error(body: &str) -> String {
	format!("{:#}", compile(body).expect_err("should be rejected"))
}

fn constant(def: &str) -> Result<TypeMap> {
	let reg = pds2::registry();
	let ast = pds2_core::ast::parse(&format!("{DECL} {def}")).map_err(|e| anyhow!("{e}"))?;
	mir::from_hir(&pds2_core::hir::parse_ast(ast, &reg)?, &reg)
}

#[test]
fn records_and_lists_construct_values() {
	constant("def(c) => const(value: Pair(a: 1, b: 300));").unwrap();
	constant("def(c) => const(value: [on, (level: 2), Mode(on)]) => array(count: 3) => Mode;")
		.unwrap();
	let err = |def: &str| format!("{:#}", constant(def).expect_err("should be rejected"));
	assert!(err("def(c) => const(value: Pair(a: 1));").contains("needs field"));
	assert!(err("def(c) => const(value: Pair(a: 1, b: 2, c: 3));").contains("no stored field"));
	assert!(err("def(c) => const(value: Pair(a: \"x\", b: 2));").contains("can't be written as"));
	assert!(err("def(c) => const(value: Pair(a: 256, b: 2));").contains("can't be written as"));
	assert!(err("def(c) => const(value: Pair(a: -1, b: 2));").contains("can't be written as"));
	assert!(err("def(c) => const(value: Mode(off));").contains("no variant"));
	assert!(err("def(c) => const(value: Mode(level));").contains("carries a value"));
	assert!(err("def(c) => const(value: Mode(on: ()));").contains("carries no value"));
	assert!(err("def(c) => const(value: (a: 1, b: 2));").contains("bind `Value: <type>`"));
}

#[test]
fn serialized_needs_typed_values_with_encodings() {
	let err = |def: &str| format!("{:#}", constant(def).expect_err("should be rejected"));
	let e = err("def(c) => const(value: serialized((a: 1, b: 2))) => u8;");
	assert!(e.contains("needs a type"), "{e}");
	let e = err("def(c) => const(value: serialized(Pair(a: 1, b: 2))) => u8;");
	assert!(e.contains("u8 has no encoding"), "{e}");
}

fn counts(types: &TypeMap) -> Vec<Expr> {
	types
		.0
		.values()
		.filter_map(|d| match &d.kind {
			TypeKind::Alias(t) => Some(t),
			_ => None,
		})
		.flat_map(|t| t.args.iter().map(|(_, e)| e.clone()))
		.collect()
}

#[test]
fn arithmetic_operands_share_a_type() {
	let err = error("field(a) => u8; field(b) => u16; field(c) => array(count: ../a + ../b) => u8;");
	assert!(err.contains("different types"), "{err}");
	compile("field(a) => u8; field(b) => u16; field(c) => array(count: u16(../a) + ../b) => u8;")
		.unwrap();
}

#[test]
fn casts_need_a_conversion() {
	let err = error("field(a) => u8; field(c) => array(count: Pick(../a)) => u8;");
	assert!(err.contains("can't be cast"), "{err}");
}

#[test]
fn typed_arguments_construct_constants_and_check_fields() {
	compile("field(p) => Pick(k: 1);").unwrap();
	let err = error("field(k) => u16; field(p) => Pick(k: ../k);");
	assert!(err.contains("expected u8, got u16"), "{err}");
}

#[test]
fn generic_argument_types_are_inferred_from_typed_values() {
	compile("field(w) => terminated(end: u8(0)) => u8;").unwrap();
	compile("field(w) => terminated(End: u8, end: 0) => u8;").unwrap();
	compile("field(p) => AnyPick(k: u16(1));").unwrap();
	let err = error("field(w) => terminated(end: 0) => u8;");
	assert!(err.contains("bind `End: <type>`"), "{err}");
	let err = error("field(w) => terminated(End: u8, end: u16(0)) => u8;");
	assert!(err.contains("expected u8, got u16"), "{err}");
}

#[test]
fn constants_fold() {
	let types = compile("field(c) => array(count: 2 * 3 + 1) => u8;").unwrap();
	let folded = counts(&types)
		.into_iter()
		.chain(types.0.values().flat_map(|d| d.exprs().into_iter().cloned()))
		.any(|e| matches!(e, Expr::Const(Value::Signed(7))));
	assert!(folded);
}
