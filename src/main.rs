#![feature(drain_filter, btree_drain_filter)]

use anyhow::anyhow;
use clap::Parser;
use dprint_core::formatting::PrintOptions;
use format::SpecializerObj;
use generator::GeneratorTypes;
use jrsonnet_evaluator::{typed::Typed, FileImportResolver, State};
use std::path::PathBuf;
pub mod ast;
pub mod format;
pub mod generator;
pub mod hir;
pub mod mir;
pub mod upgrade;

#[macro_export]
macro_rules! should_be_desugared {
	($v:expr) => {
		unreachable!("should be desugared: {:?}", $v)
	};
}

pub mod pds {
	#![allow(non_camel_case_types)]
	use std::io::Cursor;

	macro_rules! impl_for_num {
        (@single $n:ident => $ty:ident) => {
            pub mod $n {
                use super::{Output, Input, Result};
                use std::io::{Read, Write};

                pub type Ty = $ty;
                const SIZE: usize = (Ty::BITS / 8) as usize;

                pub fn size_of(_val: &Ty) -> usize {
                    SIZE
                }

                pub fn serialize(val: &Ty, output: &mut Output) -> Result<()> {
                    output.0.write_all(&Ty::to_be_bytes(*val))?;
                    Ok(())
                }

                pub fn deserialize(input: &mut Input) -> Result<Ty> {
                    let mut out = [0; SIZE];
                    input.0.read_exact(&mut out)?;
                    Ok(Ty::from_be_bytes(out))
                }
            }
        };
        ($($n:ident => $ty:ident),*) => {
            $(
                impl_for_num!(@single $n => $ty);
            )*
        };
    }
	impl_for_num!(
		pds_i8 => i8,
		pds_u8 => u8,
		pds_i16 => i16,
		pds_u16 => u16,
		pds_i32 => i32,
		pds_u32 => u32,
		pds_i64 => i64,
		pds_u64 => u64,
		pds_i128 => i128,
		pds_u128 => u128
	);
	macro_rules! impl_for_le_num {
        (@single $n:ident => $ty:ident) => {
            pub mod $n {
                use super::{Output, Input, Result};
                use std::io::{Read, Write};

                pub type Ty = $ty;
                const SIZE: usize = (Ty::BITS / 8) as usize;

                pub fn size_of(_val: &Ty) -> usize {
                    SIZE
                }

                pub fn serialize(val: &Ty, output: &mut Output) -> Result<()> {
                    output.0.write_all(&Ty::to_le_bytes(*val))?;
                    Ok(())
                }

                pub fn deserialize(input: &mut Input) -> Result<Ty> {
                    let mut out = [0; SIZE];
                    input.0.read_exact(&mut out)?;
                    Ok(Ty::from_le_bytes(out))
                }
            }
        };
        ($($n:ident => $ty:ident),*) => {
            $(
                impl_for_le_num!(@single $n => $ty);
            )*
        };
    }
	impl_for_le_num!(
		pds_i8_le => i8,
		pds_u8_le => u8,
		pds_i16_le => i16,
		pds_u16_le => u16,
		pds_i32_le => i32,
		pds_u32_le => u32,
		pds_i64_le => i64,
		pds_u64_le => u64,
		pds_i128_le => i128,
		pds_u128_le => u128
	);

	pub use pds_u64 as pds_usize;

	#[derive(Default)]
	pub struct Input(Cursor<Vec<u8>>);
	#[derive(Default)]
	pub struct Output(pub Vec<u8>);
	pub type Result<T> = anyhow::Result<T>;
	// #[test]
	// fn test() {
	//     let mut out = Output::default();
	//     let data = arrayreftest {
	//         field: arrayreftest_field {
	//             arr: vec![1, 2, 3, 4],
	//         },
	//     };
	// }
}

#[derive(Parser)]
enum Opts {
	/// Compile .pds definition to code
	Compile {
		/// .pds source code
		from: PathBuf,
		/// Generation target
		generator: GeneratorTypes,
		/// Print AST
		#[clap(long)]
		print_ast: bool,
		/// Print generated HIR
		#[clap(long)]
		print_hir: bool,
		/// Print generated MIR
		#[clap(long)]
		print_mir: bool,

		/// Where to write generated files to
		#[clap(long)]
		to: Option<PathBuf>,
		/// Definition of where to look for specializers
		#[clap(long)]
		specializer: Option<PathBuf>,
	},
	/// Convert protodef .json definition to code
	Upgrade {
		/// .json source code
		from: PathBuf,
	},
	/// Reformat .pds definition code
	Format { from: PathBuf },
}

trait Desugar<Pass> {
	fn desugar(&mut self, pass: &mut Pass);
}

#[macro_export]
macro_rules! pi {
    (@single $pi:ident => {if $v:expr => $($body:tt)*}) => {
        if $v {
            pi!($pi => $($body)*);
        }
    };
    (@single $pi:ident => {ifelse $v:expr => {$($body:tt)*} {$($elsebody:tt)*}}) => {
        if $v {
            pi!($pi => $($body)*);
        } else {
            pi!($pi => $($elsebody)*);
        }
    };
    (@single $pi:ident => {forin $v:ident in $e:expr => $($body:tt)*}) => {
        for $v in $e {
            pi!($pi => $($body)*);
        }
    };
    (@single $pi:ident => $li:literal) => {
        $pi.push_str($li);
    };
    (@single $pi:ident => {<$v:expr}) => {
        $pi.extend($v);
    };
    (@single $pi:ident => {=$v:expr}) => {
        $pi.push_string(format!("{}", $v))
    };
    (@single $pi:ident => {?$v:expr}) => {
        $pi.push_string(format!("{:?}", $v))
    };
    (@single $pi:ident => NL) => {
        $pi.push_signal(dprint_core::formatting::Signal::NewLine)
    };
    (@single $pi:ident => INDENT) => {
        $pi.push_signal(dprint_core::formatting::Signal::StartIndent)
    };
    (@single $pi:ident => DEDENT) => {
        $pi.push_signal(dprint_core::formatting::Signal::FinishIndent)
    };
    ($pi:ident => $($tt:tt)+) => {{
        $(
            pi!(@single $pi => $tt);
        )+
    }};
}

fn main() -> anyhow::Result<()> {
	let opts = Opts::parse();
	match opts {
		Opts::Compile {
			from,
			generator,
			print_ast,
			print_hir,
			print_mir,

			to,
			specializer,
		} => {
			let data = std::fs::read_to_string(from)?;
			let parsed = ast::parse(&data)?;

			if print_ast {
				dbg!(&parsed);
			}

			let hir = hir::parse_ast(parsed)?;

			if print_hir {
				dbg!(&hir);
			}

			let mir = mir::from_hir(&hir)?;

			if print_mir {
				dbg!(&mir);
			}

			let state = State::default();
			state.set_import_resolver(Box::new(FileImportResolver::new(Vec::new())));
			let specializer = if let Some(specializer) = specializer {
				SpecializerObj::from_untyped(state.import(specializer).map_err(|e| anyhow!("{e}"))?)
					.map_err(|e| anyhow!("{e}"))?
			} else {
				SpecializerObj::default()
			};

			let mut out = Default::default();
			generator.generator()(&mir, &mut out, &specializer)?;

			if let Some(to) = &to {
				std::fs::create_dir_all(&to)?;
			}
			for (_file, value) in out {
				let mut file = to.clone();
				if let Some(file) = &mut file {
					file.push(_file);
				}
				let out = dprint_core::formatting::format(
					|| value,
					PrintOptions {
						max_width: 100,
						indent_width: 4,
						use_tabs: true,
						new_line_text: "\n",
					},
				);
				let out = format!("// This file is automatically generated\n{out}");

				if let Some(file) = file {
					std::fs::write(file, out)?;
				} else {
                    println!("{out}");
                }
			}
		}
		Opts::Format { from } => {
			let data = std::fs::read_to_string(from)?;
			let parsed = ast::parse(&data)?;
			let out = ast::format(&parsed);
			println!("{}", out);
		}
		Opts::Upgrade { from } => {
			let data = std::fs::read_to_string(from)?;
			let parsed = serde_json::from_str(&data)?;
			let upgraded = upgrade::upgrade(parsed)?;
			let out = ast::format(&upgraded);
			println!("{}", out);
		}
	}
	Ok(())
}
