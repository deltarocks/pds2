use genco::lang::{JavaScript, Rust};
use pds2_core::specializer::Registry;
use pds2_specializer_bytes::register_encoder;

mod pipe;
mod resource;
mod result;
mod service;

pub use service::{Method, MethodKind, ServiceData, service_id};

pub fn register(reg: &mut Registry) {
	reg.register_definition("service", service::Service);
	reg.register_layout("resource", resource::ResourceNative);
	reg.register_codegen::<Rust>("resource", resource::ResourceNative);
	reg.register_codegen::<JavaScript>("resource", resource::ResourceNative);
	register_encoder(reg, "resource", resource::ResourceNative);
	reg.register_layout("pipe", pipe::PipeNative);
	reg.register_codegen::<Rust>("pipe", pipe::PipeNative);
	reg.register_codegen::<JavaScript>("pipe", pipe::PipeNative);
	register_encoder(reg, "pipe", pipe::PipeNative);
	reg.register_layout("result", result::ResultNative);
	reg.register_codegen::<Rust>("result", result::ResultNative);
	reg.register_codegen::<JavaScript>("result", result::ResultNative);
	register_encoder(reg, "result", result::ResultNative);
}
