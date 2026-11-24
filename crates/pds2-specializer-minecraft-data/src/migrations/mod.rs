use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use pds2_core::ast::{Attributes, Item, Value};
use pds2_core::definition::{Definition, DefinitionUse, ExtraModule, LowerCtx, Lowered};
use pds2_core::generator::rust::DefinitionRustCtx;

mod lower;
mod rules;
mod rust;

pub use rules::Migration;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Dir {
	Up,
	Down,
}

impl Dir {
	fn name(self) -> &'static str {
		match self {
			Dir::Up => "up",
			Dir::Down => "down",
		}
	}
}

pub struct Version {
	pub package: String,
	pub name: String,
	pub protocols: Vec<i32>,
	pub legacy: Vec<i32>,
	pub snapshots: Vec<(i32, String)>,
}

pub struct MigrationsData {
	pub versions: Vec<Version>,
	pub steps: Vec<Option<Migration>>,
	pub every: Option<Migration>,
	pub virtual_packets: bool,
	pub base: BTreeMap<String, String>,
}

pub struct Migrations;

impl Definition for Migrations {
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
		ensure!(
			uses.len() == 1,
			"a schema holds one migrations definition, found {}",
			uses.len()
		);
		let u = uses[0];
		Ok(vec![ExtraModule {
			namespace: u.namespace.clone(),
			tokens: rust::generate(u.data()?, ctx, &u.namespace)?,
		}])
	}
}
