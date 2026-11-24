use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context as _, anyhow};
use pds2_cli::CoreCommand;
use pds2_cli::clap::{self, Parser, Subcommand};
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
		/// Snapshots closest to this definition, `<name>=<protocol>`, kept as `@!snapshots`
		#[arg(long, value_delimiter = ',', value_parser = snapshot)]
		snapshot: Vec<(String, i32)>,
	},
	/// Compare upgraded .pds files of consecutive protocol versions
	Survey {
		/// .pds files, oldest first
		versions: Vec<PathBuf>,
	},
	/// List definition changes between two upgraded .pds files
	Diff { old: PathBuf, new: PathBuf },
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

fn snapshot(arg: &str) -> Result<(String, i32), String> {
	let (name, protocol) = arg
		.rsplit_once('=')
		.ok_or_else(|| format!("expected <name>=<protocol>, got {arg:?}"))?;
	let protocol = protocol.parse().map_err(|e| format!("{arg:?}: {e}"))?;
	Ok((name.to_owned(), protocol))
}

fn main() {
	pds2_cli::main(|| {
		match Opts::parse() {
			Opts::Core(cmd) => cmd.run(&pds2::registry())?,
			Opts::MinecraftData(MinecraftData::Upgrade {
				from,
				to,
				doc,
				protocol,
				snapshot,
			}) => {
				let data = std::fs::read_to_string(&from)?;
				let parsed = serde_json::from_str(&data)?;
				let mut upgraded =
					pds2_specializer_protodef::upgrade(parsed, &pds2::minecraft_data_upgraders())?;
				if let Some(doc) = doc {
					upgraded.attributes.insert(
						ast::Ident::new("doc"),
						vec![ast::Value::String {
							string: doc,
							is_block: false,
						}],
					);
				}
				if !protocol.is_empty() {
					upgraded.attributes.insert(
						ast::Ident::new("protocol"),
						vec![ast::Value::List(
							protocol
								.iter()
								.map(|p| ast::Value::Int(*p as i128))
								.collect(),
						)],
					);
				}
				if !snapshot.is_empty() {
					upgraded.attributes.insert(
						ast::Ident::new("snapshots"),
						vec![ast::Value::List(
							snapshot
								.into_iter()
								.map(|(name, protocol)| {
									ast::Value::Record(vec![
										(
											ast::Ident::new("protocol"),
											ast::Value::Int(protocol.into()),
										),
										(
											ast::Ident::new("name"),
											ast::Value::String {
												string: name,
												is_block: false,
											},
										),
									])
								})
								.collect(),
						)],
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
					let block =
						ast::parse(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
					let name = path
						.file_stem()
						.map(|s| s.to_string_lossy().into_owned())
						.unwrap_or_default();
					loaded.push(pds2::survey::load(&name, &block));
				}
				print!("{}", pds2::survey::report(&loaded));
			}
			Opts::MinecraftData(MinecraftData::Diff { old, new }) => {
				let load = |path: &PathBuf| -> anyhow::Result<pds2::survey::Version> {
					let data = std::fs::read_to_string(path)?;
					let block =
						ast::parse(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
					let name = path
						.file_stem()
						.map(|s| s.to_string_lossy().into_owned())
						.unwrap_or_default();
					Ok(pds2::survey::load(&name, &block))
				};
				print!("{}", pds2::survey::diff(&load(&old)?, &load(&new)?));
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
