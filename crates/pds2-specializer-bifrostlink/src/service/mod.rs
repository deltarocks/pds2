use anyhow::Result;
use pds2_core::ast::{Attributes, Ident, Item, Path, Value};
use pds2_core::definition::{Definition, DefinitionUse, ExtraModule, LowerCtx, Lowered};
use pds2_core::generator::GeneratorOutput;
use pds2_core::generator::rust::DefinitionRustCtx;
use pds2_core::mir;
use sha3::{Digest, Sha3_256};

mod lower;
mod rust;
mod typescript;

pub struct Service;

#[derive(Debug)]
pub struct ServiceData {
	pub name: Ident,
	pub namespace: Vec<Ident>,
	pub full_name: String,
	pub methods: Vec<Method>,
}

impl ServiceData {
	pub fn id(&self) -> u32 {
		service_id(&self.full_name)
	}
}

#[derive(Debug)]
pub struct Method {
	pub name: Ident,
	pub id: u64,
	pub kind: MethodKind,
	pub request: Path,
	pub response: Option<Path>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
	Request { cancel: bool },
	Notification { blocking: bool },
}

pub fn service_id(full_name: &str) -> u32 {
	let digest = Sha3_256::digest(full_name.as_bytes());
	u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

impl Definition for Service {
	fn lower(
		&self,
		attributes: Attributes,
		item: Item,
		chain: Vec<Value>,
		ctx: &LowerCtx,
	) -> Result<Lowered> {
		lower::lower(attributes, item, chain, ctx)
	}
	fn generate_rust(
		&self,
		uses: &[&DefinitionUse],
		ctx: &DefinitionRustCtx,
	) -> Result<Vec<ExtraModule>> {
		rust::generate(uses, ctx)
	}
	fn generate_typescript(
		&self,
		uses: &[&DefinitionUse],
		types: &mir::TypeMap,
		output: &mut GeneratorOutput,
	) -> Result<()> {
		typescript::generate(uses, types, output)
	}
}
