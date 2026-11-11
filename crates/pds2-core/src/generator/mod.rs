use anyhow::{bail, Result};
use std::{collections::HashMap, path::PathBuf, str::FromStr};

use crate::mir;
use crate::specializer::Registry;

mod boxing;
pub mod common;
pub mod naming;
pub mod recursion;
pub mod split;
mod views;
pub mod rust;
pub mod typescript;

pub type GeneratorOutput = HashMap<PathBuf, String>;
pub type Generator =
	fn(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()>;

#[derive(Debug, Clone, Copy)]
pub enum GeneratorTypes {
	Rust,
	Typescript,
}

impl GeneratorTypes {
	pub fn generator(&self) -> Generator {
		match self {
			GeneratorTypes::Rust => rust::generator,
			GeneratorTypes::Typescript => typescript::generator,
		}
	}
}
