use genco::lang::Rust;
use pds2_core::specializer::Registry;
use pds2_specializer_bytes::register_encoder;

mod resource;
mod service;

pub use service::{service_id, Method, MethodKind, ServiceData};

pub fn register(reg: &mut Registry) {
	reg.register_definition("service", service::Service);
	reg.register_layout("resource", resource::ResourceNative);
	reg.register_codegen::<Rust>("resource", resource::ResourceNative);
	register_encoder(reg, "resource", resource::ResourceNative);
}
