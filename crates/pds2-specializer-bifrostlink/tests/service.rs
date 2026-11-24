use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use pds2_core::generator::split::Split;
use pds2_core::generator::{rust, typescript};
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

fn compile_ts(schema: &str) -> Result<BTreeMap<PathBuf, String>> {
	let reg = registry();
	let (block, uses) = definition::lower(ast::parse(schema)?, &reg)?;
	let types = mir::from_hir(&hir::parse_ast(block, &reg)?, &reg)?;
	let mut out = Default::default();
	typescript::generator(&types, &mut out, &reg)?;
	definition::generate_typescript(&types, &uses, &reg, &mut out)?;
	Ok(out.into_iter().collect())
}

fn compile_split(
	schema: &str,
	packages: &[&str],
	module: &str,
) -> Result<BTreeMap<PathBuf, String>> {
	let reg = registry();
	let (block, uses) = definition::lower(ast::parse(schema)?, &reg)?;
	let types = mir::from_hir(&hir::parse_ast(block, &reg)?, &reg)?;
	let split = Split {
		name: "app".to_owned(),
		prelude: String::new(),
		dependencies: vec![],
		root_doc: None,
		namespace_docs: BTreeMap::new(),
		versions: BTreeMap::new(),
		packages: packages.iter().map(|p| (*p).to_owned()).collect(),
		modules: [(module.to_owned(), PathBuf::from("/host/src/link"))].into(),
	};
	let mut out = Default::default();
	rust::generate(&types, &mut out, &reg, None, Some(&split), &uses)?;
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
		assert!(
			names.iter().any(|n| n == expected),
			"{expected} missing from {names:?}"
		);
	}
	assert!(
		!names
			.iter()
			.any(|n| n == "root::remowt::Pty::resize::Response")
	);
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
fn generates_typescript_server_and_client() {
	let out = compile_ts(SCHEMA).unwrap();
	let service = &out[&PathBuf::from("remowt/Pty/service.ts")];
	let module = squash(service);
	for expected in [
		"import * as pds$remowt$Pty$open_shell$Request from \"./open_shell/Request.ts\";".to_owned(),
		"import {Call, Remote, Rpc} from \"../../../pds.ts\";".to_owned(),
		"export const SERVICE_NAME = \"remowt::Pty\";".to_owned(),
		format!("export const SERVICE_ID = {:#010x};", service_id("remowt::Pty")),
		"\"open_shell\": 1n,".to_owned(),
		"\"exited\": 3n,".to_owned(),
		"open_shell(from: A, request: pds$remowt$Pty$open_shell$Request.Ty, signal: AbortSignal): Promise<pds$remowt$Pty$open_shell$Response.Ty> | pds$remowt$Pty$open_shell$Response.Ty;".to_owned(),
		"exited(from: A, request: pds$remowt$Pty$exited$Request.Ty): Promise<void> | void;".to_owned(),
		"rpc.registerRequest(SERVICE_ID, method.open_shell, true,".to_owned(),
		"rpc.registerRequest(SERVICE_ID, method.resize, false,".to_owned(),
		"rpc.registerNotification(SERVICE_ID, method.exited, true,".to_owned(),
		"return call.encode(() => {});".to_owned(),
		"resize(request: pds$remowt$Pty$resize$Request.Ty, signal?: AbortSignal): Promise<void> {".to_owned(),
		"(output) => pds$remowt$Pty$resize$Request.serialize(request, output), () => undefined, signal);".to_owned(),
		"exited(request: pds$remowt$Pty$exited$Request.Ty): void {".to_owned(),
	] {
		assert!(module.contains(&squash(&expected)), "{expected:?} missing from:\n{service}");
	}
	let index = &out[&PathBuf::from("remowt/Pty/mod.ts")];
	assert!(
		index.contains("export * as open_shell from \"./open_shell/mod.ts\";"),
		"{index}"
	);
	assert!(
		index.ends_with("export * from \"./service.ts\";\n"),
		"{index}"
	);
}

#[test]
fn typescript_client_omits_empty_requests() {
	let out = compile_ts(&format!(
		"{NATIVES}service(S) {{ request(a, id: 1) {{}} => u8; notification(b, id: 2) {{}}; }};"
	))
	.unwrap();
	let service = squash(&out[&PathBuf::from("S/service.ts")]);
	assert!(
		service.contains(&squash(
			"a(signal?: AbortSignal): Promise<pds$S$a$Response.Ty> {"
		)),
		"{service}"
	);
	assert!(
		service.contains(&squash("pds$S$a$Request.serialize({}, output)")),
		"{service}"
	);
	assert!(service.contains(&squash("b(): void {")), "{service}");
	assert!(
		service.contains(&squash("import {Call, Remote, Rpc} from \"../../pds.ts\";")),
		"{service}"
	);
}

#[test]
fn typescript_client_names_are_reserved() {
	let e = match compile_ts(&format!(
		"{NATIVES}service(S) {{ notification(remote, id: 1) {{}}; }};"
	)) {
		Ok(_) => panic!("compiled"),
		Err(e) => format!("{e:#}"),
	};
	assert!(
		e.contains("the name `remote` is taken by the generated client"),
		"{e}"
	);
}

#[test]
fn method_needs_id() {
	let e = err("service(S) { request(a) { field(x) => u8; }; };");
	assert!(
		e.contains("`request a` needs an explicit `id: <number>`"),
		"{e}"
	);
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
	let e = err(
		"def(Request) => container { field(x) => u8; }; service(S) { request(a, id: 1) { field(x) => Request; }; };",
	);
	assert!(
		e.contains("would refer to the method's own lowered type"),
		"{e}"
	);
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
			seen.insert(service_id(&name), name.clone())
				.map(|other| (other, name))
		})
		.unwrap();
	let e = err(&format!(
		"service({a}) {{ notification(x, id: 0) {{}}; }}; service({b}) {{ notification(x, id: 0) {{}}; }};"
	));
	assert!(
		e.contains(&format!("services {a} and {b} have the same id")),
		"{e}"
	);
}

const PACKAGED: &str = "def(Thing) { export; } => container { field(x) => u8; }; package(link) { service(Front) { request(ask, id: 1) { field(thing) => Thing; } => u8; }; };";

#[test]
fn service_types_live_in_a_module_package() {
	let out = compile_split(&format!("{NATIVES}{PACKAGED}"), &["link"], "link").unwrap();
	let at = |file: &str| -> &String {
		out.get(Path::new("/host/src/link").join(file).as_path())
			.unwrap_or_else(|| panic!("{file} missing from {:?}", out.keys()))
	};
	assert!(at("mod.rs").contains("pub mod front;"), "{}", at("mod.rs"));
	let request = at("front/ask/request.rs");
	assert!(request.contains("::app::Thing"), "{request}");
	let service = squash(at("front/mod.rs"));
	assert!(
		service.contains(&squash("pub trait Server<C: ::bifrostlink::Config>")),
		"{service}"
	);
	assert!(
		!out.contains_key(&PathBuf::from("app-link/Cargo.toml")),
		"{:?}",
		out.keys()
	);
	let root = &out[&PathBuf::from("app-root/src/lib.rs")];
	assert!(!root.contains("front"), "{root}");
}

#[test]
fn types_using_a_module_package_follow_it() {
	let schema = format!(
		"{NATIVES}package(link) {{ def(Inner) => container {{ field(x) => u8; }}; }}; def(Outer) => container {{ field(i) => link::Inner; }};"
	);
	let out = compile_split(&schema, &["link"], "link").unwrap();
	assert!(
		out.contains_key(&PathBuf::from("/host/src/link/outer.rs")),
		"{:?}",
		out.keys()
	);
}

#[test]
fn module_package_types_stay_in_their_package() {
	let direct = format!(
		"{NATIVES}package(link) {{ def(Inner) => container {{ field(x) => u8; }}; }}; package(other) {{ def(Outer) => container {{ field(i) => link::Inner; }}; }};"
	);
	let e = format!(
		"{:#}",
		compile_split(&direct, &["link", "other"], "link").unwrap_err()
	);
	assert!(
		e.contains("other_Outer uses link_Inner of module package link"),
		"{e}"
	);
	let through_root = format!(
		"{NATIVES}package(link) {{ def(Inner) => container {{ field(x) => u8; }}; }}; def(Middle) => container {{ field(i) => link::Inner; }}; package(other) {{ def(Outer) => container {{ field(m) => Middle; }}; }};"
	);
	let e = format!(
		"{:#}",
		compile_split(&through_root, &["link", "other"], "link").unwrap_err()
	);
	assert!(e.contains("which only that package can use"), "{e}");
}
