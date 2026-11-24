use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use genco::lang::js;
use genco::prelude::*;
use pds2_core::ast::Path;
use pds2_core::definition::DefinitionUse;
use pds2_core::generator::GeneratorOutput;
use pds2_core::generator::naming::{self, Modules};
use pds2_core::hir::TypeId;
use pds2_core::mir::{TypeKind, TypeMap};
use pds2_specializer_protodef::container;

use super::{Method, MethodKind, ServiceData};

const CLIENT_RESERVED: &[&str] = &["constructor", "remote"];

struct Located<'a> {
	types: &'a TypeMap,
	modules: Modules,
	by_path: BTreeMap<&'a Path, TypeId>,
}

impl<'a> Located<'a> {
	fn new(types: &'a TypeMap) -> Result<Self> {
		Ok(Self {
			types,
			modules: naming::raw_modules(types)?,
			by_path: types.0.iter().map(|(id, def)| (&def.name, *id)).collect(),
		})
	}
	fn type_id(&self, path: &Path) -> Result<TypeId> {
		self.by_path
			.get(path)
			.copied()
			.ok_or_else(|| anyhow!("no generated type {path:?}"))
	}
	fn dir(&self, id: TypeId) -> Vec<String> {
		let path = self.modules.path(id);
		path[..path.len() - 1]
			.iter()
			.map(|m| m.stem.clone())
			.collect()
	}
	fn module(&self, here: &[String], id: TypeId) -> js::Import {
		let path = self.modules.path(id);
		let (dir, file) = path.split_at(path.len() - 1);
		let here: Vec<&str> = here.iter().map(String::as_str).collect();
		let dir: Vec<&str> = dir.iter().map(|m| m.stem.as_str()).collect();
		let (up, rest) = naming::relative(&here, &dir);
		let prefix = if up == 0 {
			"./".to_owned()
		} else {
			"../".repeat(up)
		};
		let target = rest
			.into_iter()
			.chain([file[0].stem.as_str()])
			.collect::<Vec<_>>()
			.join("/");
		let alias = std::iter::once("pds")
			.chain(path.iter().map(|m| m.ident.as_str()))
			.collect::<Vec<_>>()
			.join("$");
		js::import(format!("{prefix}{target}.ts"), alias).into_wildcard()
	}
}

struct Gen {
	name: String,
	kind: MethodKind,
	id: u64,
	request: js::Import,
	empty: bool,
	response: Option<js::Import>,
}

pub fn generate(
	uses: &[&DefinitionUse],
	types: &TypeMap,
	output: &mut GeneratorOutput,
) -> Result<()> {
	let services = uses
		.iter()
		.map(|u| u.data::<ServiceData>())
		.collect::<Result<Vec<_>>>()?;
	let mut ids: BTreeMap<u32, &str> = BTreeMap::new();
	for s in &services {
		if let Some(other) = ids.insert(s.id(), &s.full_name) {
			bail!(
				"services {other} and {} have the same id {:#010x}",
				s.full_name,
				s.id()
			);
		}
	}
	let located = Located::new(types)?;
	for s in services {
		service(s, &located, output).with_context(|| format!("service {}", s.full_name))?;
	}
	Ok(())
}

fn method(m: &Method, here: &[String], located: &Located) -> Result<Gen> {
	ensure!(
		!CLIENT_RESERVED.contains(&m.name.as_str()),
		"the name `{}` is taken by the generated client",
		m.name
	);
	let request = located.type_id(&m.request)?;
	let def = &located.types[request];
	ensure!(
		def.params.is_empty(),
		"request of {} can't take arguments",
		m.name
	);
	let TypeKind::Native(native) = &def.kind else {
		bail!("request of {} is not a container", m.name);
	};
	let fields = container::view(native)
		.ok_or_else(|| anyhow!("request of {} must be a protodef container", m.name))?;
	let response = m
		.response
		.as_ref()
		.map(|path| -> Result<js::Import> {
			let id = located.type_id(path)?;
			ensure!(
				located.types[id].params.is_empty(),
				"response of {} can't take arguments",
				m.name
			);
			Ok(located.module(here, id))
		})
		.transpose()?;
	Ok(Gen {
		name: m.name.as_str().to_owned(),
		kind: m.kind,
		id: m.id,
		request: located.module(here, request),
		empty: fields.iter().all(|f| f.load_from.is_some()),
		response,
	})
}

fn service(s: &ServiceData, located: &Located, output: &mut GeneratorOutput) -> Result<()> {
	let first = s
		.methods
		.first()
		.ok_or_else(|| anyhow!("service {} has no methods", s.name))?;
	let first = located.dir(located.type_id(&first.request)?);
	let here = first[..first.len() - 1].to_vec();
	let mut seen = BTreeSet::new();
	let methods = s
		.methods
		.iter()
		.map(|m| {
			ensure!(
				seen.insert(m.name.as_str()),
				"method {} is defined twice",
				m.name
			);
			method(m, &here, located).with_context(|| format!("method {}", m.name))
		})
		.collect::<Result<Vec<_>>>()?;

	let up = "../".repeat(here.len() + 1);
	let runtime = |item: &str| js::import(format!("{up}pds.ts"), item);
	let rpc = &runtime("Rpc");
	let remote = &runtime("Remote");
	let call = &runtime("Call");

	let constants = methods
		.iter()
		.map(|g| quote!($(quoted(&g.name)): $(format!("{}n", g.id)),));
	let signatures = methods.iter().map(signature);
	let registrations = methods.iter().map(|g| registration(g, call));
	let calls = methods.iter().map(client_call);

	let tokens: js::Tokens = quote! {
		export const SERVICE_NAME = $(quoted(s.full_name.as_str()));
		export const SERVICE_ID = $(format!("{:#010x}", s.id()));
		export const method = {
			$(for c in constants join ($['\r']) => $c)
		} as const;
		export interface Server<A> {
			$(for s in signatures join ($['\r']) => $s)
		}
		export function register_endpoints<A>(server: Server<A>, rpc: $rpc<A>): void {
			rpc.registerService(SERVICE_ID, SERVICE_NAME);
			$(for r in registrations join ($['\r']) => $r)
		}
		export class Client<A> {
			constructor(readonly remote: $remote<A>) {}
			$(for c in calls join ($['\r']) => $c)
		}
	};
	let dir = here.join("/");
	let file = |name: &str| -> PathBuf {
		if dir.is_empty() {
			name.into()
		} else {
			format!("{dir}/{name}").into()
		}
	};
	let service_file = file("service.ts");
	ensure!(
		!output.contains_key(&service_file),
		"{} is already generated",
		service_file.display()
	);
	output.insert(
		service_file,
		tokens.to_file_string().context("formatting the service")?,
	);
	let index = output.entry(file("mod.ts")).or_default();
	if !index.is_empty() && !index.ends_with('\n') {
		index.push('\n');
	}
	index.push_str("export * from \"./service.ts\";\n");
	Ok(())
}

fn response_ty(g: &Gen) -> js::Tokens {
	match &g.response {
		Some(r) => quote!($r.Ty),
		None => quote!(void),
	}
}

fn signature(g: &Gen) -> js::Tokens {
	let name = &g.name;
	let request = &g.request;
	match g.kind {
		MethodKind::Request { .. } => {
			let response = response_ty(g);
			quote!($name(from: A, request: $request.Ty, signal: AbortSignal): Promise<$(&response)> | $(&response);)
		}
		MethodKind::Notification { .. } => {
			quote!($name(from: A, request: $request.Ty): Promise<void> | void;)
		}
	}
}

fn registration(g: &Gen, call: &js::Import) -> js::Tokens {
	let name = &g.name;
	let request = &g.request;
	match g.kind {
		MethodKind::Request { cancel } => {
			let encode = match &g.response {
				Some(r) => quote!(call.encode((output) => $r.serialize(response, output))),
				None => quote!(call.encode(() => {})),
			};
			quote! {
				rpc.registerRequest(SERVICE_ID, method.$name, $(cancel.to_string()), async (from: A, call: $call<A>, signal: AbortSignal) => {
					const request = call.decode($request.deserialize);
					const response = await server.$name(from, request, signal);
					return $encode;
				});
			}
		}
		MethodKind::Notification { blocking } => quote! {
			rpc.registerNotification(SERVICE_ID, method.$name, $(blocking.to_string()), async (from: A, call: $call<A>) => {
				const request = call.decode($request.deserialize);
				await server.$name(from, request);
			});
		},
	}
}

fn client_call(g: &Gen) -> js::Tokens {
	let name = &g.name;
	let request = &g.request;
	let (mut params, value) = if g.empty {
		(vec![], quote!({}))
	} else {
		(vec![quote!(request: $request.Ty)], quote!(request))
	};
	match g.kind {
		MethodKind::Request { .. } => {
			let decode = match &g.response {
				Some(r) => quote!($r.deserialize),
				None => quote!(() => undefined),
			};
			params.push(quote!(signal?: AbortSignal));
			quote! {
				$name($(for p in params join (, ) => $p)): Promise<$(response_ty(g))> {
					return this.remote.request(SERVICE_ID, method.$name, (output) => $request.serialize($value, output), $decode, signal);
				}
			}
		}
		MethodKind::Notification { .. } => {
			quote! {
				$name($(for p in params join (, ) => $p)): void {
					this.remote.notify(SERVICE_ID, method.$name, (output) => $request.serialize($value, output));
				}
			}
		}
	}
}
