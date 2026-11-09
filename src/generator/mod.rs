use anyhow::{bail, Result};
use clap::ValueEnum;
use std::{collections::HashMap, path::PathBuf, str::FromStr};

use crate::mir;
use crate::specializer::Registry;

mod boxing;
mod common;
mod rust;
mod typescript;

pub type GeneratorOutput = HashMap<PathBuf, String>;
pub type Generator =
	fn(input: &mir::TypeMap, output: &mut GeneratorOutput, reg: &Registry) -> Result<()>;

#[derive(Debug, ValueEnum, Clone)]
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
