use clap::Parser;
use generator::GeneratorTypes;
use std::io;
use std::path::PathBuf;
use tracing::info;

use self::specializer::{register_protodef_types, Registry};
pub mod ast;
pub mod format;
pub mod generator;
pub mod hir;
pub mod mir;
pub mod proto;
pub mod specializer;
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

		/// Add `def(NAME) => const(value: <FILE contents>);`, FILE being a single expression
		#[clap(long = "const", value_name = "NAME=FILE")]
		consts: Vec<String>,

		/// Where to write generated files to
		#[clap(long)]
		to: Option<PathBuf>,
	},
	/// Convert protodef .json definition to code
	Upgrade {
		/// .json source code
		from: PathBuf,
		/// Where to write the generated .pds (defaults to `<from>.pds`)
		#[clap(long)]
		to: Option<PathBuf>,
	},
	/// Reformat .pds definition code
	Format { from: PathBuf },
	/// Convert a protobuf FileDescriptorSet (`protoc --include_imports --descriptor_set_out`)
	Proto {
		from: PathBuf,
		/// .proto files (as named in the set) whose types, and everything they reach, to convert
		#[clap(long)]
		root: Vec<String>,
		#[clap(long)]
		to: PathBuf,
	},
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

fn main() {
	if let Err(err) = try_main() {
		let mut chain = err.chain().rev();
		eprintln!("ERROR {}", chain.next().unwrap());
		for ele in chain {
			eprintln!("   at {ele}");
		}
		std::process::exit(1);
	}
}

fn try_main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.without_time()
		.with_writer(io::stderr)
		.init();
	let opts = Opts::parse();

	let mut reg = Registry::default();
	register_protodef_types(&mut reg);

	match opts {
		Opts::Compile {
			from,
			generator,
			print_ast,
			print_hir,
			print_mir,
			consts,
			to,
		} => {
			let data = std::fs::read_to_string(from)?;
			let mut parsed = ast::parse(&data)?;
			for c in consts {
				let (name, file) = c
					.split_once('=')
					.ok_or_else(|| anyhow::anyhow!("--const {c:?} should be NAME=FILE"))?;
				let value = std::fs::read_to_string(file)?;
				ast::parse_expr(&value).map_err(|e| anyhow::anyhow!("parsing {file}: {e}"))?;
				let def = ast::parse(&format!("def({name}) => const(value: {value});"))
					.map_err(|e| anyhow::anyhow!("--const {name:?}: {e}"))?;
				parsed.statements.extend(def.statements);
			}

			if print_ast {
				dbg!(&parsed);
			}

			let hir = hir::parse_ast(parsed, &reg)?;

			if print_hir {
				dbg!(&hir);
			}

			let mir = mir::from_hir(&hir, &reg)?;

			if print_mir {
				dbg!(&mir);
			}

			let mut out = Default::default();
			generator.generator()(&mir, &mut out, &reg)?;

			if let Some(to) = &to {
				std::fs::create_dir_all(to)?;
			}
			for (_file, value) in out {
				let mut file = to.clone();
				if let Some(file) = &mut file {
					file.push(_file);
				}
				let out = value; /*dprint_core::formatting::format(
						 || value,
						 PrintOptions {
							 max_width: 100,
							 indent_width: 4,
							 use_tabs: true,
							 new_line_text: "\n",
						 },
					 );*/
				let out =
					format!("// This file is automatically generated from PDS descriptor\n{out}");

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
		Opts::Proto { from, root, to } => {
			let data = std::fs::read(&from)?;
			let set = protobuf_descriptor::decode(&data)
				.map_err(|e| anyhow::anyhow!("decoding {}: {e:?}", from.display()))?;
			std::fs::write(&to, proto::convert(&set, &root)?)?;
			info!("wrote {}", to.display());
		}
		Opts::Upgrade { from, to } => {
			let data = std::fs::read_to_string(&from)?;
			let parsed = serde_json::from_str(&data)?;
			let upgraded = upgrade::upgrade(parsed, &reg)?;
			let out = ast::format(&upgraded);

			let to = to.unwrap_or_else(|| from.with_extension("pds"));
			std::fs::write(&to, out)?;
			info!("wrote upgraded {}", to.display());
		}
	}
	Ok(())
}
