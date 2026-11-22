use anyhow::{Result, anyhow};

use pds2_core::mir::{self, TypeMap};

const SCHEMA: &str = include_str!("../../csn1-tests/csn1.pds");

fn compile(def: &str) -> Result<TypeMap> {
	let reg = pds2::registry();
	let ast = pds2_core::ast::parse(&format!("{SCHEMA} {def}")).map_err(|e| anyhow!("{e}"))?;
	mir::from_hir(&pds2_core::hir::parse_ast(ast, &reg)?, &reg)
}

fn err(def: &str) -> String {
	format!("{:#}", compile(def).expect_err("should be rejected"))
}

const SYNTHETIC: &str = "Synthetic((kind: none, header: (), n: 1, items: [ITEM], reserved: (), deltas: [], flag: false))";

#[test]
fn the_schema_compiles() {
	compile("").unwrap();
}

#[test]
fn choices_must_tell_their_arms_apart() {
	let e = err(
		r#"def(Bad) => choice { variant(a, bits: "0"); variant(b, bits: "01"); };
		def(bad) => const(value: Bad(deserialized_bits(b"\x00")));"#,
	);
	assert!(e.contains("can't be told apart"), "{e}");
	let e = err(
		r#"def(Bad) => choice { variant(a, bits: "L"); variant(b, bits: "1"); };
		def(bad) => const(value: Bad(deserialized_bits(b"\x00")));"#,
	);
	assert!(e.contains("can't be told apart"), "{e}");
}

#[test]
fn decoding_bits_rejects_whole_octets_left_over() {
	let e = err(
		r#"def(x) => const(value: rr::Si3RestOctets(deserialized_bits(b"\x2b\x2b\x2b\x2b\x00")));"#,
	);
	assert!(e.contains("8 bits are left"), "{e}");
}

#[test]
fn values_must_fit_their_bits() {
	let value = SYNTHETIC.replace("ITEM", "16");
	let e = err(&format!(
		"def(x) => const(value: Synthetic(deserialized_bits(serialized_bits({value}))));"
	));
	assert!(e.contains("doesn't fit the 4 bits"), "{e}");
	let value = SYNTHETIC.replace("ITEM", "15");
	compile(&format!(
		"def(x) => const(value: Synthetic(deserialized_bits(serialized_bits({value}))));"
	))
	.unwrap();
}

#[test]
fn byte_functions_refuse_bit_encodings() {
	let value = SYNTHETIC.replace("ITEM", "1");
	let e = err(&format!(
		"@scalar bytes def_native(octets); def(x) => const(value: octets(serialized({value})));"
	));
	assert!(e.contains("doesn't write bytes"), "{e}");
}
