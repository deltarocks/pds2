use genco::lang::{JavaScript, Rust};
use pds2_core::specializer::Registry;
use pds2_specializer_bytes::register_encoder;

mod constant;
mod terminated;
mod ts;

pub use constant::Const;
pub use terminated::Terminated;

pub fn register_terminated(reg: &mut Registry, name: &str) {
	reg.register_asthir(name, Terminated);
	reg.register_layout(name, Terminated);
	reg.register_codegen::<Rust>(name, Terminated);
	reg.register_codegen::<JavaScript>(name, Terminated);
	reg.register_value(name, Terminated);
	register_encoder(reg, name, Terminated);
}

pub fn register(reg: &mut Registry) {
	reg.register_asthir("const", Const);
	reg.register_layout("const", Const);
	reg.register_codegen::<Rust>("const", Const);
	reg.register_codegen::<JavaScript>("const", Const);
	reg.register_value("const", Const);
	register_encoder(reg, "const", Const);
	register_terminated(reg, "terminated");
}
