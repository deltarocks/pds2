use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{anyhow, Context as _};
use pds2_cli::clap::{self, Parser, Subcommand};
use pds2_cli::CoreCommand;
use pds2_core::ast;
use tracing::info;

#[derive(Parser)]
enum Opts {
	#[command(flatten)]
	Core(CoreCommand),
	/// minecraft-data protocol definitions
	#[command(subcommand)]
	MinecraftData(MinecraftData),
	/// Protocol buffers
	#[command(subcommand)]
	Protobuf(Protobuf),
}

#[derive(Subcommand)]
enum MinecraftData {
	/// Convert protodef .json definition to code
	Upgrade {
		/// .json source code
		from: PathBuf,
		/// Where to write the generated .pds (defaults to `<from>.pds`)
		#[arg(long)]
		to: Option<PathBuf>,
		/// `@!doc` text for the top of the file
		#[arg(long)]
		doc: Option<String>,
		/// Protocol numbers served by this definition, kept as `@!protocol`
		#[arg(long, value_delimiter = ',')]
		protocol: Vec<i32>,
	},
	/// Compare upgraded .pds files of consecutive protocol versions
	Survey {
		/// .pds files, oldest first
		versions: Vec<PathBuf>,
	},
	/// List definition changes between two upgraded .pds files
	Diff { old: PathBuf, new: PathBuf },
	/// Generate converters between consecutive versions of a merged schema
	Converters {
		merged: PathBuf,
		/// directory with `<old>-<new>.pds` migration files
		#[arg(long)]
		migrations: PathBuf,
		/// generated code directory of the merged schema
		#[arg(long)]
		gen: PathBuf,
		/// only report missing rules, write nothing
		#[arg(long)]
		check: bool,
		/// let crate::custom carry packets through steps as decoded models (virtual_start/step/finish)
		#[arg(long = "virtual")]
		virtual_packets: bool,
	},
	/// Merge upgraded .pds files of consecutive versions into one schema
	Merge {
		/// .pds files, oldest first; the file name is the version
		versions: Vec<PathBuf>,
		#[arg(long)]
		to: PathBuf,
	},
}

#[derive(Subcommand)]
enum Protobuf {
	/// Convert a protobuf FileDescriptorSet (`protoc --include_imports --descriptor_set_out`)
	Convert {
		from: PathBuf,
		/// .proto files (as named in the set) whose types, and everything they reach, to convert
		#[arg(long)]
		root: Vec<String>,
		#[arg(long)]
		to: PathBuf,
	},
	/// Encode a value written in pds syntax to binary, written to stdout
	Encode {
		schema: PathBuf,
		/// Type of the value, when it isn't written as `root::Type(...)`
		#[arg(long = "type")]
		ty: Option<String>,
		value: PathBuf,
	},
	/// Decode a binary value to pds syntax, written to stdout
	Decode {
		schema: PathBuf,
		#[arg(long = "type")]
		ty: String,
		value: PathBuf,
	},
}

fn main() {
	pds2_cli::main(|| {
		match Opts::parse() {
			Opts::Core(cmd) => cmd.run(&pds2::registry())?,
			Opts::MinecraftData(MinecraftData::Upgrade { from, to, doc, protocol }) => {
				let data = std::fs::read_to_string(&from)?;
				let parsed = serde_json::from_str(&data)?;
				let mut upgraded =
					pds2_specializer_protodef::upgrade(parsed, &pds2::minecraft_data_upgraders())?;
				if let Some(doc) = doc {
					upgraded.attributes.insert(
						ast::Ident::new("doc"),
						vec![ast::Value::String { string: doc, is_block: false }],
					);
				}
				if !protocol.is_empty() {
					upgraded.attributes.insert(
						ast::Ident::new("protocol"),
						vec![ast::Value::List(protocol.iter().map(|p| ast::Value::Int(*p as i128)).collect())],
					);
				}
				let to = to.unwrap_or_else(|| from.with_extension("pds"));
				std::fs::write(&to, ast::format(&upgraded))?;
				info!("wrote upgraded {}", to.display());
			}
			Opts::MinecraftData(MinecraftData::Survey { versions }) => {
				let mut loaded = Vec::new();
				for path in &versions {
					let data = std::fs::read_to_string(path)?;
					let block = ast::parse(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
					let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
					loaded.push(pds2::survey::load(&name, &block));
				}
				print!("{}", pds2::survey::report(&loaded));
			}
			Opts::MinecraftData(MinecraftData::Diff { old, new }) => {
				let load = |path: &PathBuf| -> anyhow::Result<pds2::survey::Version> {
					let data = std::fs::read_to_string(path)?;
					let block = ast::parse(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
					let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
					Ok(pds2::survey::load(&name, &block))
				};
				print!("{}", pds2::survey::diff(&load(&old)?, &load(&new)?));
			}
			Opts::MinecraftData(MinecraftData::Merge { versions, to }) => {
				let mut inputs = Vec::new();
				for path in &versions {
					let data = std::fs::read_to_string(path)?;
					let block = ast::parse(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
					let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
					inputs.push((name, block));
				}
				let merged = pds2::merge::merge(&inputs)?;
				std::fs::write(&to, ast::format(&merged.block))?;
				let defs: usize = merged.variants.values().map(Vec::len).sum();
				let split = merged.variants.values().filter(|v| v.len() > 1).count();
				info!(
					"wrote {}: {} names, {defs} definitions, {split} names with several variants",
					to.display(),
					merged.variants.len()
				);
			}
			Opts::MinecraftData(MinecraftData::Converters { merged, migrations, gen, check, virtual_packets }) => {
				let reg = pds2::registry();
				let data = std::fs::read_to_string(&merged)?;
				let block = ast::parse(&data).map_err(|e| anyhow!("{}: {e}", merged.display()))?;
				let hir = pds2_core::hir::parse_ast(block.clone(), &reg)?;
				let mir = pds2_core::mir::from_hir(&hir, &reg)?;
				let versions = pds2::merge::versions(&block)?;
				let names: Vec<String> = versions.iter().map(|v| v.name.clone()).collect();
				let mut steps = Vec::new();
				for pair in names.windows(2) {
					let path = migrations.join(format!("{}-{}.pds", pair[0], pair[1]));
					let rules = match std::fs::read_to_string(&path) {
						Ok(text) => {
							let parsed = ast::parse(&text).map_err(|e| anyhow!("{}: {e}", path.display()))?;
							Some(pds2::converters::parse_migration(&parsed).with_context(|| path.display().to_string())?)
						}
						Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
						Err(e) => return Err(anyhow!("{}: {e}", path.display())),
					};
					steps.push(rules);
				}
				let every_path = migrations.join("every.pds");
				let every = match std::fs::read_to_string(&every_path) {
					Ok(text) => {
						let parsed = ast::parse(&text).map_err(|e| anyhow!("{}: {e}", every_path.display()))?;
						Some(pds2::converters::parse_every(&parsed).with_context(|| every_path.display().to_string())?)
					}
					Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
					Err(e) => return Err(anyhow!("{}: {e}", every_path.display())),
				};
				let code = pds2::converters::generate(&mir, &reg, &block, &versions, &steps, every.as_ref(), virtual_packets)?;
				if check {
					return Ok(());
				}
				std::fs::write(
					gen.join("convert.rs"),
					format!("// This file is automatically generated from PDS migrations\n#![allow(unreachable_code)]\n{code}"),
				)?;
				let mod_rs = gen.join("mod.rs");
				let mods = std::fs::read_to_string(&mod_rs)?;
				if !mods.contains("pub mod convert;") {
					std::fs::write(&mod_rs, format!("{mods}pub mod convert;\n"))?;
				}
				let skipped: Vec<String> = names
					.windows(2)
					.zip(&steps)
					.filter(|(_, s)| s.is_none())
					.map(|(p, _)| format!("{}-{}", p[0], p[1]))
					.collect();
				if !skipped.is_empty() {
					info!("no migration, steps not generated: {}", skipped.join(" "));
				}
				info!("wrote {}", gen.join("convert.rs").display());
			}
			Opts::Protobuf(Protobuf::Convert { from, root, to }) => {
				let data = std::fs::read(&from)?;
				let set = protobuf_descriptor::decode(&data)
					.map_err(|e| anyhow!("decoding {}: {e:?}", from.display()))?;
				std::fs::write(&to, pds2_specializer_protobuf::convert(&set, &root)?)?;
				info!("wrote {}", to.display());
			}
			Opts::Protobuf(Protobuf::Encode { schema, ty, value }) => {
				let bytes = pds2_cli::encode(
					&std::fs::read_to_string(&schema)?,
					ty.as_deref(),
					&std::fs::read_to_string(&value)?,
					&pds2::registry(),
				)
				.with_context(|| format!("encoding {}", value.display()))?;
				std::io::stdout().write_all(&bytes)?;
			}
			Opts::Protobuf(Protobuf::Decode { schema, ty, value }) => {
				let text = pds2_cli::decode(
					&std::fs::read_to_string(&schema)?,
					&ty,
					&std::fs::read(&value)?,
					&pds2::registry(),
				)
				.with_context(|| format!("decoding {}", value.display()))?;
				print!("{text}");
			}
		}
		Ok(())
	})
}
