use std::collections::BTreeMap;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use genco::lang::rust;
use genco::prelude::*;
use pds2_core::ast::Ident;
use pds2_core::definition::{DefinitionUse, ExtraModule};
use pds2_core::generator::naming::{Case, Scope};
use pds2_core::generator::rust::{DefinitionRustCtx, RustNames};
use pds2_core::mir::TypeKind;
use pds2_specializer_protodef::container;

use super::{Method, MethodKind, ServiceData};

const RESERVED: &[&str] = &["register_endpoints", "new", "clone"];

struct Param {
	name: String,
	ty: rust::Tokens,
	boxed: bool,
}

struct Response {
	ty: rust::Tokens,
	serialize: rust::Tokens,
	deserialize: rust::Tokens,
}

struct Gen {
	name: String,
	constant: String,
	kind: MethodKind,
	request: rust::Tokens,
	serialize: rust::Tokens,
	deserialize: rust::Tokens,
	params: Vec<Param>,
	response: Option<Response>,
}

pub fn generate(uses: &[&DefinitionUse], ctx: &DefinitionRustCtx) -> Result<Vec<ExtraModule>> {
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
	services
		.into_iter()
		.map(|s| {
			let namespace: Vec<Ident> = s
				.namespace
				.iter()
				.cloned()
				.chain([s.name.clone()])
				.collect();
			let tokens =
				service(s, &namespace, ctx).with_context(|| format!("service {}", s.full_name))?;
			Ok(ExtraModule { namespace, tokens })
		})
		.collect()
}

fn method(m: &Method, name: String, names: &RustNames, ctx: &DefinitionRustCtx) -> Result<Gen> {
	let types = ctx.types();
	let request = ctx.type_id(&m.request)?;
	let def = &types[request];
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
	let params = fields
		.iter()
		.filter(|f| f.load_from.is_none())
		.map(|f| {
			let field = names.field(request, f.name.as_str());
			ensure!(
				field != "from",
				"{}: a request field can't be named `from`",
				m.name
			);
			Ok(Param {
				name: field,
				ty: names.ty(f.ty.target),
				boxed: names.boxed(request, f.name),
			})
		})
		.collect::<Result<Vec<_>>>()?;
	let response = m
		.response
		.as_ref()
		.map(|path| -> Result<Response> {
			let id = ctx.type_id(path)?;
			ensure!(
				types[id].params.is_empty(),
				"response of {} can't take arguments",
				m.name
			);
			Ok(Response {
				ty: names.ty(id),
				serialize: names.serialize(id),
				deserialize: names.deserialize(id),
			})
		})
		.transpose()?;
	Ok(Gen {
		constant: name.trim_start_matches("r#").to_uppercase(),
		name,
		kind: m.kind,
		request: names.ty(request),
		serialize: names.serialize(request),
		deserialize: names.deserialize(request),
		params,
		response,
	})
}

fn service(s: &ServiceData, namespace: &[Ident], ctx: &DefinitionRustCtx) -> Result<rust::Tokens> {
	let names = ctx.names_in(namespace)?;
	let scope = Scope::new(
		Case::Snake,
		s.methods.iter().map(|m| m.name.as_str()),
		RESERVED,
	)
	.context("naming methods")?;
	let methods = s
		.methods
		.iter()
		.map(|m| {
			method(m, scope.get(m.name.as_str()), &names, ctx)
				.with_context(|| format!("method {}", m.name))
		})
		.collect::<Result<Vec<_>>>()?;

	let constants = s
		.methods
		.iter()
		.zip(&methods)
		.map(|(m, g)| quote!(pub const $(&g.constant): u64 = $(m.id.to_string());));
	let signatures = methods.iter().map(signature);
	let registrations = methods.iter().map(registration);
	let calls = methods.iter().map(call);

	Ok(quote! {
		pub const SERVICE_NAME: &str = $(quoted(s.full_name.as_str()));
		pub const SERVICE_ID: u32 = $(format!("{:#010x}", s.id()));
		pub mod method {
			$(for c in constants join ($['\r']) => $c)
		}
		pub trait Server<C: ::bifrostlink::Config>: Send + Sync + 'static {
			$(for s in signatures join ($['\r']) => $s)
			fn register_endpoints(self, rpc: &::bifrostlink::Rpc<C>)
			where
				Self: Sized,
			{
				let __this = ::std::sync::Arc::new(self);
				rpc.__register_service(SERVICE_ID, SERVICE_NAME);
				$(for r in registrations join ($['\r']) => $r)
			}
		}
		pub struct Client<C: ::bifrostlink::Config> {
			remote: ::bifrostlink::Remote<C>,
		}
		impl<C: ::bifrostlink::Config> ::core::clone::Clone for Client<C> {
			fn clone(&self) -> Self {
				Self { remote: self.remote.clone() }
			}
		}
		impl<C: ::bifrostlink::Config> ::core::convert::From<::bifrostlink::Remote<C>> for Client<C> {
			fn from(remote: ::bifrostlink::Remote<C>) -> Self {
				Self { remote }
			}
		}
		impl<C: ::bifrostlink::Config> Client<C> {
			pub fn new(remote: ::bifrostlink::Remote<C>) -> Self {
				Self { remote }
			}
			$(for c in calls join ($['\r']) => $c)
		}
	})
}

fn params(g: &Gen) -> rust::Tokens {
	quote!($(for p in &g.params => , $(&p.name): $(&p.ty)))
}

fn response_ty(g: &Gen) -> rust::Tokens {
	match &g.response {
		Some(r) => r.ty.clone(),
		None => quote!(()),
	}
}

fn signature(g: &Gen) -> rust::Tokens {
	let output = match g.kind {
		MethodKind::Request { .. } => {
			quote!(::core::result::Result<$(response_ty(g)), C::Error>)
		}
		MethodKind::Notification { .. } => quote!(()),
	};
	quote! {
		fn $(&g.name)(&self, from: C::Address$(params(g))) -> impl ::core::future::Future<Output = $output> + Send;
	}
}

fn registration(g: &Gen) -> rust::Tokens {
	let locals: Vec<String> = (0..g.params.len()).map(|i| format!("__{i}")).collect();
	let pattern = quote!($(&g.request) { $(for (p, l) in g.params.iter().zip(&locals) join (, ) => $(&p.name): $l) });
	let args = quote!($(for (p, l) in g.params.iter().zip(&locals) => , $(if p.boxed => *)$l));
	let constant = &g.constant;
	let deserialize = &g.deserialize;
	let name = &g.name;
	let (register, flag, body) = match g.kind {
		MethodKind::Request { cancel } => {
			let encode = match &g.response {
				Some(r) => quote!(__call.encode(|__output| $(&r.serialize)(&__response, __output))),
				None => quote!(__call.encode(|_| ::core::result::Result::Ok(()))),
			};
			(
				"__register_request",
				cancel,
				quote! {
					let $pattern = __call.decode($deserialize)?;
					let __response = __this.$name(__from$args).await?;
					$encode
				},
			)
		}
		MethodKind::Notification { blocking } => (
			"__register_notification",
			blocking,
			quote! {
				let ::core::result::Result::Ok($pattern) = __call.decode($deserialize) else {
					return;
				};
				__this.$name(__from$args).await
			},
		),
	};
	quote! {
		{
			let __this = __this.clone();
			rpc.$register(SERVICE_ID, method::$constant, $(flag.to_string()), move |__from: C::Address, __call: ::bifrostlink::__private::Call<C>| {
				let __this = __this.clone();
				async move {
					$body
				}
			});
		}
	}
}

fn call(g: &Gen) -> rust::Tokens {
	let build = g.params.iter().map(|p| {
		if p.boxed {
			quote!($(&p.name): ::std::boxed::Box::new($(&p.name)))
		} else {
			quote!($(&p.name))
		}
	});
	let request = quote!(let __request = $(&g.request) { $(for b in build join (, ) => $b) };);
	let constant = &g.constant;
	let serialize = &g.serialize;
	let name = &g.name;
	match g.kind {
		MethodKind::Request { .. } => {
			let decode = match &g.response {
				Some(r) => r.deserialize.clone(),
				None => quote!(|_| ::core::result::Result::Ok(())),
			};
			quote! {
				pub async fn $name(&self$(params(g))) -> ::core::result::Result<$(response_ty(g)), C::Error> {
					$request
					self.remote
						.__request(SERVICE_ID, method::$constant, |__output| $serialize(&__request, __output), $decode)
						.await
				}
			}
		}
		MethodKind::Notification { .. } => quote! {
			pub fn $name(&self$(params(g))) -> ::core::result::Result<(), C::Error> {
				$request
				self.remote
					.__notify(SERVICE_ID, method::$constant, |__output| $serialize(&__request, __output))
			}
		},
	}
}
