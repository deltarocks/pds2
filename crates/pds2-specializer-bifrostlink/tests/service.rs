use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::Result;
use pds2_core::generator::rust;
use pds2_core::specializer::Registry;
use pds2_core::{ast, definition, hir, mir};
use pds2_specializer_bifrostlink::service_id;

const SCHEMA: &str = include_str!("remowt.pds");

const NATIVES: &str = r#"
@scalar u8 @encoding be
def_native(u8);
@specializer container
def_native(container) {
	body {
		item(field) { type(Value); };
		item(virtual_field) { argument(value); type(Value); };
	};
};
"#;

fn registry() -> Registry {
	let mut reg = Registry::default();
	pds2_specializer_bytes::register(&mut reg);
	pds2_specializer_generic::register(&mut reg);
	pds2_specializer_protodef::register(&mut reg);
	pds2_specializer_bifrostlink::register(&mut reg);
	reg
}

fn types(schema: &str) -> Result<mir::TypeMap> {
	let reg = registry();
	mir::from_hir(&hir::parse_ast(ast::parse(schema)?, &reg)?, &reg)
}

fn compile(schema: &str) -> Result<BTreeMap<PathBuf, String>> {
	let reg = registry();
	let (block, uses) = definition::lower(ast::parse(schema)?, &reg)?;
	let types = mir::from_hir(&hir::parse_ast(block, &reg)?, &reg)?;
	let mut out = Default::default();
	rust::generate(&types, &mut out, &reg, None, None, &uses)?;
	Ok(out.into_iter().collect())
}

fn err(schema: &str) -> String {
	match compile(&format!("{NATIVES}{schema}")) {
		Ok(_) => panic!("schema compiled: {schema}"),
		Err(e) => format!("{e:#}"),
	}
}

fn squash(s: &str) -> String {
	s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn service_id_is_sha3_prefix() {
	assert_eq!(service_id("abc"), 0x3a985da7);
}

#[test]
fn lowers_to_request_and_response_types() {
	let types = types(SCHEMA).unwrap();
	let names: Vec<String> = types.0.values().map(|d| format!("{:?}", d.name)).collect();
	for expected in [
		"root::remowt::Pty::open_shell::Request",
		"root::remowt::Pty::open_shell::Response",
		"root::remowt::Pty::resize::Request",
		"root::remowt::Pty::exited::Request",
	] {
		assert!(names.iter().any(|n| n == expected), "{expected} missing from {names:?}");
	}
	assert!(!names.iter().any(|n| n == "root::remowt::Pty::resize::Response"));
}

#[test]
fn generates_server_and_client() {
	let out = compile(SCHEMA).unwrap();
	let module = squash(&out[&PathBuf::from("remowt/pty/mod.rs")]);
	for expected in [
		"pub const SERVICE_NAME: &str = \"remowt::Pty\";".to_owned(),
		format!("pub const SERVICE_ID: u32 = {:#010x};", service_id("remowt::Pty")),
		"pub const OPEN_SHELL: u64 = 1;".to_owned(),
		"pub const EXITED: u64 = 3;".to_owned(),
		"pub trait Server<C: ::bifrostlink::Config>: Send + Sync + 'static".to_owned(),
		"fn open_shell(&self, from: C::Address, tunnel:".to_owned(),
		"rpc.__register_service(SERVICE_ID, SERVICE_NAME);".to_owned(),
		"rpc.__register_request(SERVICE_ID, method::OPEN_SHELL, true,".to_owned(),
		"rpc.__register_request(SERVICE_ID, method::RESIZE, false,".to_owned(),
		"rpc.__register_notification(SERVICE_ID, method::EXITED, true,".to_owned(),
		"pub struct Client<C: ::bifrostlink::Config>".to_owned(),
		"impl<C: ::bifrostlink::Config> ::core::convert::From<::bifrostlink::Remote<C>> for Client<C>".to_owned(),
		"pub async fn open_shell(&self, tunnel:".to_owned(),
		"pub fn exited(&self, id:".to_owned(),
	] {
		assert!(module.contains(&squash(&expected)), "{expected:?} missing from:\n{}", out[&PathBuf::from("remowt/pty/mod.rs")]);
	}
	assert!(out.contains_key(&PathBuf::from("remowt/pty/open_shell/request.rs")));
}

#[test]
fn method_needs_id() {
	let e = err("service(S) { request(a) { field(x) => u8; }; };");
	assert!(e.contains("`request a` needs an explicit `id: <number>`"), "{e}");
}

#[test]
fn method_ids_are_unique() {
	let e = err("service(S) { request(a, id: 1) {}; notification(b, id: 1) {}; };");
	assert!(e.contains("method id 1 is used by both a and b"), "{e}");
}

#[test]
fn method_names_are_unique() {
	let e = err("service(S) { request(a, id: 1) {}; request(a, id: 2) {}; };");
	assert!(e.contains("method a is defined twice"), "{e}");
}

#[test]
fn notification_has_no_response() {
	let e = err("service(S) { notification(a, id: 1) {} => u8; };");
	assert!(e.contains("notification a has no response"), "{e}");
}

#[test]
fn flags_belong_to_their_kind() {
	let e = err("service(S) { request(a, id: 1, blocking: true) {}; };");
	assert!(e.contains("unused argument blocking"), "{e}");
}

#[test]
fn lowered_names_are_not_shadowed() {
	let e = err("def(Request) => container { field(x) => u8; }; service(S) { request(a, id: 1) { field(x) => Request; }; };");
	assert!(e.contains("would refer to the method's own lowered type"), "{e}");
}

#[test]
fn service_has_methods() {
	let e = err("service(S) {};");
	assert!(e.contains("service S has no methods"), "{e}");
}

#[test]
fn method_name_is_not_reserved() {
	let e = err("service(S) { request(method, id: 1) {}; };");
	assert!(e.contains("the name `method` is taken"), "{e}");
}

#[test]
fn unknown_service_statement() {
	let e = err("service(S) { bogus(x); };");
	assert!(e.contains("unknown statement `bogus` in a service"), "{e}");
}

#[test]
fn request_field_is_not_from() {
	let e = err("service(S) { request(a, id: 1) { field(from) => u8; }; };");
	assert!(e.contains("a request field can't be named `from`"), "{e}");
}

#[test]
fn service_ids_are_unique() {
	let mut seen = HashMap::new();
	let (a, b) = (0u32..)
		.find_map(|n| {
			let name = format!("S{n}");
			seen.insert(service_id(&name), name.clone()).map(|other| (other, name))
		})
		.unwrap();
	let e = err(&format!(
		"service({a}) {{ notification(x, id: 0) {{}}; }}; service({b}) {{ notification(x, id: 0) {{}}; }};"
	));
	assert!(e.contains(&format!("services {a} and {b} have the same id")), "{e}");
}
