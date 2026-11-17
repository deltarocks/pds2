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

fn exprs(types: &TypeMap) -> Vec<Expr> {
	let mut out = counts(types);
	out.extend(types.0.values().flat_map(|d| d.exprs().into_iter().cloned()));
	out
}

fn with_fns(fns: &str, body: &str) -> Result<TypeMap> {
	constant(&format!("{fns} def(T) {{ export; }} => container {{ {body} }};"))
}

fn fn_error(fns: &str, body: &str) -> String {
	format!("{:#}", with_fns(fns, body).expect_err("should be rejected"))
}

#[test]
fn functions_inline_at_call_sites() {
	let types = with_fns(
		"fn(name, arg1, arg2: u8) => arg2 * 2;",
		"field(n) => u8; field(c) => array(count: name(../n, ../n)) => u8;",
	)
	.unwrap();
	assert!(exprs(&types)
		.iter()
		.any(|e| matches!(e, Expr::Binary(pds2_core::ast::BinOp::Mul, a, _) if a.as_field().is_some())));
	let types = with_fns(
		"fn(add, a, b) => a + b; fn(twice, n) => add(n, n);",
		"field(c) => array(count: twice(add(2, 3))) => u8;",
	)
	.unwrap();
	assert!(exprs(&types)
		.iter()
		.any(|e| matches!(e, Expr::Const(Value::Signed(10)))));
}

#[test]
fn typed_parameters_check_their_values() {
	let fns = "fn(wide, n: u16) => n * 2;";
	let err = fn_error(fns, "field(n) => u8; field(c) => array(count: wide(../n)) => u8;");
	assert!(err.contains("expected u16, got u8"), "{err}");
	with_fns(fns, "field(n) => u8; field(c) => array(count: wide(u16(../n))) => u8;").unwrap();
	with_fns(fns, "field(c) => array(count: wide(3)) => u8;").unwrap();
	with_fns("fn(zero, n: u8) => n;", "field(w) => terminated(end: zero(0)) => u8;").unwrap();
}

#[test]
fn typed_parameters_carry_arguments_and_outer_refs() {
	constant(
		"fn(wide, n: u16) => n * 2;
		def(W) { type(V); argument(n) => u16; } => container { field(xs) => array(count: wide(../$n)) => V; };
		def(T) { export; } => container {
			field(m) => u16;
			field(w) => W(V: u8, n: ../m);
			field(k) => u8;
			field(u) => union(tag: ../k) { variant(a, match: 1) => array(count: wide(u16(../../k))) => u8; };
		};",
	)
	.unwrap();
}

#[test]
fn function_bodies_resolve_in_their_namespace() {
	let types = with_fns(
		"@scalar u8 def_native(w); namespace(a) { @scalar u16 def_native(w); fn(widen, n) => w(n); };",
		"field(n) => u8; field(c) => array(count: a::widen(../n)) => u8;",
	)
	.unwrap();
	let target = exprs(&types).into_iter().find_map(|e| match e {
		Expr::Cast(_, to) => Some(types[to].name.readable_name()),
		_ => None,
	});
	assert_eq!(target.as_deref(), Some("a_w"));
	constant(
		"fn(g, x) => [x, on]; fn(f, on) => g((level: 2));
		def(c) => const(value: f(5)) => array(count: 2) => Mode;",
	)
	.unwrap();
}

#[test]
fn bad_functions_are_rejected() {
	let body = "field(c) => array(count: f(1)) => u8;";
	let err = fn_error("fn(f, n) => n + ../x;", body);
	assert!(err.contains("sees only its parameters"), "{err}");
	let err = fn_error("fn(f, n) => g(n); fn(g, n) => f(n) + 1;", body);
	assert!(err.contains("calls itself"), "{err}");
	let err = fn_error("fn(f, n) => nope(n);", body);
	assert!(err.contains("not a type or function"), "{err}");
	let err = fn_error("fn(f, a, b) => a + b;", body);
	assert!(err.contains("takes 2 values, got 1"), "{err}");
	let err = fn_error("fn(f, n, n) => n;", body);
	assert!(err.contains("declared twice"), "{err}");
	let err = fn_error("fn(f, n) => n; fn(f, m) => m;", body);
	assert!(err.contains("already defined"), "{err}");
	let err = fn_error("fn(u8, n) => n;", body);
	assert!(err.contains("both as a function and an item"), "{err}");
	let err = fn_error("", "field(c) => array(count: nope(1, 2)) => u8;");
	assert!(err.contains("not a declared or registered function"), "{err}");
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
