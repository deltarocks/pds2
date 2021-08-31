use anyhow::{bail, Result};
use dprint_core::formatting::PrintItems;
use std::{collections::HashMap, path::PathBuf, str::FromStr};

use crate::mir;

mod rust;

pub type GeneratorOutput = HashMap<PathBuf, PrintItems>;
pub type Generator = fn(input: &mir::TypeMap, output: &mut GeneratorOutput) -> Result<()>;

#[derive(Debug)]
pub enum GeneratorTypes {
    Rust,
}

impl GeneratorTypes {
    pub fn generator(&self) -> Generator {
        match self {
            GeneratorTypes::Rust => rust::generator,
        }
    }
}

impl FromStr for GeneratorTypes {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "rust" => Ok(Self::Rust),
            n => bail!("unknown generator type: {}", n),
        }
    }
}
