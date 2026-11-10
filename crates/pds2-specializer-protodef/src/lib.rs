use genco::lang::{JavaScript, Rust};
use pds2_core::specializer::Registry;

pub mod container;
mod natives;
mod ts;
pub mod union;
pub mod upgrade;
mod upgraders;

use container::Container;
use natives::{Array, Bitfield, Bitflags, Buffer, OptionP, PString};
use union::Union;
pub use upgrade::{upgrade, Upgrade, UpgradeRegistry};
use upgraders::*;

pub fn register(reg: &mut Registry) {
	reg.register_asthir("container", Container);
	reg.register_layout("container", Container);
	reg.register_codegen::<Rust>("container", Container);
	reg.register_codegen::<JavaScript>("container", Container);
	reg.register_value("container", Container);
	reg.register_encoder("container", Container);

	reg.register_asthir("union", Union);
	reg.register_layout("union", Union);
	reg.register_codegen::<Rust>("union", Union);
	reg.register_codegen::<JavaScript>("union", Union);
	reg.register_value("union", Union);
	reg.register_encoder("union", Union);

	reg.register_asthir("array", Array);
	reg.register_layout("array", Array);
	reg.register_codegen::<Rust>("array", Array);
	reg.register_codegen::<JavaScript>("array", Array);
	reg.register_value("array", Array);
	reg.register_encoder("array", Array);

	reg.register_layout("option", OptionP);
	reg.register_codegen::<Rust>("option", OptionP);
	reg.register_codegen::<JavaScript>("option", OptionP);
	reg.register_value("option", OptionP);
	reg.register_encoder("option", OptionP);

	reg.register_asthir("bitfield", Bitfield);
	reg.register_layout("bitfield", Bitfield);
	reg.register_codegen::<Rust>("bitfield", Bitfield);
	reg.register_codegen::<JavaScript>("bitfield", Bitfield);
	reg.register_value("bitfield", Bitfield);
	reg.register_encoder("bitfield", Bitfield);

	reg.register_asthir("bitflags", Bitflags);
	reg.register_layout("bitflags", Bitflags);
	reg.register_codegen::<Rust>("bitflags", Bitflags);
	reg.register_codegen::<JavaScript>("bitflags", Bitflags);
	reg.register_value("bitflags", Bitflags);
	reg.register_encoder("bitflags", Bitflags);

	reg.register_layout("buffer", Buffer);
	reg.register_codegen::<Rust>("buffer", Buffer);
	reg.register_codegen::<JavaScript>("buffer", Buffer);
	reg.register_value("buffer", Buffer);
	reg.register_encoder("buffer", Buffer);

	reg.register_layout("pstring", PString);
	reg.register_codegen::<Rust>("pstring", PString);
	reg.register_codegen::<JavaScript>("pstring", PString);
	reg.register_value("pstring", PString);
	reg.register_encoder("pstring", PString);
}

pub fn register_upgraders(reg: &mut UpgradeRegistry) {
	reg.register(Union);
	reg.register(Container);
	reg.register(Array);
	reg.register(OptionP);
	reg.register(Bitfield);
	reg.register(Bitflags);
	reg.register(Buffer);
	reg.register(PString);
	reg.register(BoolP);
	reg.register(RestBuffer);
	reg.register(Switch);
	reg.register(Mapper);
	reg.register(Void);
	reg.register(U8P);
	reg.register(U16P);
	reg.register(U32P);
	reg.register(U64P);
	reg.register(I8P);
	reg.register(I16P);
	reg.register(I32P);
	reg.register(I64P);
	reg.register(F32P);
	reg.register(F64P);
	reg.register(Varint);
	reg.register(Varlong);
}
