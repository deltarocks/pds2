use genco::lang::{JavaScript, Rust};
use pds2_core::specializer::Registry;
use pds2_specializer_bytes::register_encoder;

pub mod container;
mod natives;
mod ts;
pub mod union;
pub mod upgrade;
mod upgraders;

use container::Container;
use natives::{Array, Bitfield, Bitflags, Buffer, MapP, OptionP, PString};
use union::Union;
pub use upgrade::{Upgrade, UpgradeRegistry, upgrade};
use upgraders::*;

pub fn register(reg: &mut Registry) {
	reg.register_asthir("container", Container);
	reg.register_layout("container", Container);
	reg.register_codegen::<Rust>("container", Container);
	reg.register_codegen::<JavaScript>("container", Container);
	reg.register_value("container", Container);
	register_encoder(reg, "container", Container);

	reg.register_asthir("union", Union);
	reg.register_layout("union", Union);
	reg.register_codegen::<Rust>("union", Union);
	reg.register_codegen::<JavaScript>("union", Union);
	reg.register_value("union", Union);
	register_encoder(reg, "union", Union);

	reg.register_asthir("array", Array);
	reg.register_layout("array", Array);
	reg.register_codegen::<Rust>("array", Array);
	reg.register_codegen::<JavaScript>("array", Array);
	reg.register_value("array", Array);
	register_encoder(reg, "array", Array);

	reg.register_asthir("map", MapP);
	reg.register_layout("map", MapP);
	reg.register_codegen::<Rust>("map", MapP);
	reg.register_codegen::<JavaScript>("map", MapP);
	reg.register_value("map", MapP);
	register_encoder(reg, "map", MapP);

	reg.register_layout("option", OptionP);
	reg.register_codegen::<Rust>("option", OptionP);
	reg.register_codegen::<JavaScript>("option", OptionP);
	reg.register_value("option", OptionP);
	register_encoder(reg, "option", OptionP);

	reg.register_asthir("bitfield", Bitfield);
	reg.register_layout("bitfield", Bitfield);
	reg.register_codegen::<Rust>("bitfield", Bitfield);
	reg.register_codegen::<JavaScript>("bitfield", Bitfield);
	reg.register_value("bitfield", Bitfield);
	register_encoder(reg, "bitfield", Bitfield);

	reg.register_asthir("bitflags", Bitflags);
	reg.register_layout("bitflags", Bitflags);
	reg.register_codegen::<Rust>("bitflags", Bitflags);
	reg.register_codegen::<JavaScript>("bitflags", Bitflags);
	reg.register_value("bitflags", Bitflags);
	register_encoder(reg, "bitflags", Bitflags);

	reg.register_layout("buffer", Buffer);
	reg.register_codegen::<Rust>("buffer", Buffer);
	reg.register_codegen::<JavaScript>("buffer", Buffer);
	reg.register_value("buffer", Buffer);
	register_encoder(reg, "buffer", Buffer);

	reg.register_layout("pstring", PString);
	reg.register_codegen::<Rust>("pstring", PString);
	reg.register_codegen::<JavaScript>("pstring", PString);
	reg.register_value("pstring", PString);
	register_encoder(reg, "pstring", PString);
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
