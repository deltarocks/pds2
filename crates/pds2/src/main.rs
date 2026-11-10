use std::path::PathBuf;

use anyhow::anyhow;
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
}

fn main() {
	pds2_cli::main(|| {
		match Opts::parse() {
			Opts::Core(cmd) => cmd.run(&pds2::registry())?,
			Opts::MinecraftData(MinecraftData::Upgrade { from, to }) => {
				let data = std::fs::read_to_string(&from)?;
				let parsed = serde_json::from_str(&data)?;
				let upgraded =
					pds2_specializer_protodef::upgrade(parsed, &pds2::minecraft_data_upgraders())?;
				let to = to.unwrap_or_else(|| from.with_extension("pds"));
				std::fs::write(&to, ast::format(&upgraded))?;
				info!("wrote upgraded {}", to.display());
			}
			Opts::Protobuf(Protobuf::Convert { from, root, to }) => {
				let data = std::fs::read(&from)?;
				let set = protobuf_descriptor::decode(&data)
					.map_err(|e| anyhow!("decoding {}: {e:?}", from.display()))?;
				std::fs::write(&to, pds2_specializer_protobuf::convert(&set, &root)?)?;
				info!("wrote {}", to.display());
			}
		}
		Ok(())
	})
}
