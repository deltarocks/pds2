use dprint_core::formatting::PrintOptions;
use generator::GeneratorTypes;
use std::{fmt::Debug, path::PathBuf};
use structopt::clap::AppSettings::*;
use structopt::StructOpt;
mod a;
pub mod ast;
pub mod generator;
pub mod hir;
pub mod mir;
pub mod upgrade;

pub mod pds {
    #![allow(non_camel_case_types)]
    use std::io::Cursor;

    macro_rules! impl_for_num {
        (@single $n:ident => $ty:ident) => {
            mod $n {
                use super::{Output, Input, Result};
                use std::io::{Read, Write};

                pub type Ty = $ty;
                const SIZE: usize = (Ty::BITS / 8) as usize;

                fn size_of(val: &Ty) -> usize {
                    SIZE
                }

                fn serialize(val: &Ty, output: &mut Output) -> Result<()> {
                    output.0.write_all(&Ty::to_be_bytes(*val));
                    Ok(())
                }

                fn deserialize(input: &mut Input) -> Result<Ty> {
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
            mod $n {
                use super::{Output, Input, Result};
                use std::io::{Read, Write};

                pub type Ty = $ty;
                const SIZE: usize = (Ty::BITS / 8) as usize;

                fn size_of(val: &Ty) -> usize {
                    SIZE
                }

                fn serialize(val: &Ty, output: &mut Output) -> Result<()> {
                    output.0.write_all(&Ty::to_le_bytes(*val));
                    Ok(())
                }

                fn deserialize(input: &mut Input) -> Result<Ty> {
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

#[derive(StructOpt, Debug)]
#[structopt(setting(ColorAuto), setting(ColoredHelp))]
enum Opts {
    /// Compile .pds definition to code
    #[structopt(setting(ColorAuto), setting(ColoredHelp))]
    Compile {
        /// .pds source code
        from: PathBuf,
        /// Generation target
        generator: GeneratorTypes,
    },
    /// Convert .json definition to code
    #[structopt(setting(ColorAuto), setting(ColoredHelp))]
    Upgrade {
        /// .json source code
        from: PathBuf,
    },
    /// Reformat .pds definition code
    #[structopt(setting(ColorAuto), setting(ColoredHelp))]
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
        $pi.push_str(&format!("{}", $v))
    };
    (@single $pi:ident => {?$v:expr}) => {
        $pi.push_str(&format!("{:?}", $v))
    };
    (@single $pi:ident => NL) => {
        $pi.push_signal(Signal::NewLine)
    };
    (@single $pi:ident => INDENT) => {
        $pi.push_signal(Signal::StartIndent)
    };
    (@single $pi:ident => DEDENT) => {
        $pi.push_signal(Signal::FinishIndent)
    };
    ($pi:ident => $($tt:tt)+) => {{
        $(
            pi!(@single $pi => $tt);
        )+
    }};
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::from_args();
    match opts {
        Opts::Compile { from, generator } => {
            let data = std::fs::read_to_string(&from)?;
            let parsed = ast::parse(&data)?;
            let hir = hir::parse_ast(parsed)?;
            let mir = mir::from_hir(&hir)?;

            let mut out = Default::default();
            generator.generator()(&mir, &mut out)?;

            for (file, value) in out {
                let out = dprint_core::formatting::format(
                    || value,
                    PrintOptions {
                        max_width: 100,
                        indent_width: 4,
                        use_tabs: true,
                        new_line_text: "\n",
                    },
                );
                println!("// File: {:?}", file);
                println!("{}", out);
            }
        }
        Opts::Format { from } => {
            let data = std::fs::read_to_string(&from)?;
            let parsed = ast::parse(&data)?;
            let out = ast::format(&parsed);
            println!("{}", out);
        }
        Opts::Upgrade { from } => {
            let data = std::fs::read_to_string(&from)?;
            let parsed = serde_json::from_str(&data)?;
            let upgraded = upgrade::upgrade(parsed)?;
            let out = ast::format(&upgraded);
            println!("{}", out);
        },
    }
    Ok(())
}
