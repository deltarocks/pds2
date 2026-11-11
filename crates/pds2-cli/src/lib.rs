use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use pds2_core::generator::rust;
use pds2_core::generator::split::Split;
use pds2_core::generator::GeneratorTypes;
use pds2_core::specializer::encode::Encoding;
use pds2_core::specializer::expr::{source, Elaborator};
use pds2_core::specializer::Registry;
use pds2_core::{ast, hir, mir};

pub use clap;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Target {
	Rust,
	Typescript,
}

impl From<Target> for GeneratorTypes {
	fn from(t: Target) -> Self {
		match t {
			Target::Rust => GeneratorTypes::Rust,
			Target::Typescript => GeneratorTypes::Typescript,
		}
	}
}

#[derive(Debug, Args)]
pub struct CompileArgs {
	/// .pds source code
	pub from: PathBuf,
	/// Generation target
	pub generator: Target,
	/// Print AST
	#[arg(long)]
	pub print_ast: bool,
	/// Print generated HIR
	#[arg(long)]
	pub print_hir: bool,
	/// Print generated MIR
	#[arg(long)]
	pub print_mir: bool,

	/// Add `def(NAME) => const(value: <FILE contents>);`, FILE being a single expression
	#[arg(long = "const", value_name = "NAME=FILE")]
	pub consts: Vec<String>,

	/// Where to write generated files to
	#[arg(long)]
	pub to: Option<PathBuf>,

	/// Also generate borrowing views (`view/`) of the types under NAMESPACE (rust only)
	#[arg(long, value_name = "NAMESPACE")]
	pub view: Option<String>,

	/// Write a crate per top-level namespace (NAME-<namespace>), NAME-root for the rest and an
	/// umbrella crate NAME re-exporting them all (rust only). They link each other as workspace
	/// dependencies and take edition and license from [workspace.package], so the workspace has to
	/// declare every one of them and those keys. Crates already in the output directory keep their
	/// version, new ones start at 0.1.0; descriptions come from the `@!doc` of each namespace and
	/// of the root
	#[arg(long, value_name = "NAME")]
	pub crates: Option<String>,

	/// Rust items put at the top of every generated crate root, it has to provide `pds` and `pds_native`
	#[arg(long, value_name = "FILE", requires = "crates")]
	pub crate_prelude: Option<PathBuf>,

	/// Workspace dependency of every generated crate
	#[arg(long = "crate-dependency", value_name = "NAME", requires = "crates")]
	pub crate_dependencies: Vec<String>,
}

#[derive(Debug, Args)]
pub struct FormatArgs {
	pub from: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum CoreCommand {
	/// Compile .pds definition to code
	Compile(CompileArgs),
	/// Reformat .pds definition code
	Format(FormatArgs),
}

impl CoreCommand {
	pub fn run(self, reg: &Registry) -> Result<()> {
		match self {
			CoreCommand::Compile(args) => compile(args, reg),
			CoreCommand::Format(args) => format(args),
		}
	}
}

pub fn compile(args: CompileArgs, reg: &Registry) -> Result<()> {
	let CompileArgs {
		from,
		generator,
		print_ast,
		print_hir,
		print_mir,
		consts,
		to,
		view,
		crates,
		crate_prelude,
		crate_dependencies,
	} = args;
	let data = std::fs::read_to_string(from)?;
	let mut parsed = ast::parse(&data)?;
	let split = match crates {
		Some(name) => Some(Split {
			name,
			root_doc: doc(&parsed.attributes),
			namespace_docs: namespace_docs(&parsed),
			versions: to.as_deref().map(existing_versions).unwrap_or_default(),
			prelude: match &crate_prelude {
				Some(file) => std::fs::read_to_string(file)
					.map_err(|e| anyhow!("{}: {e}", file.display()))?,
				None => String::new(),
			},
			dependencies: crate_dependencies,
		}),
		None => None,
	};
	for c in consts {
		let (name, file) = c
			.split_once('=')
			.ok_or_else(|| anyhow!("--const {c:?} should be NAME=FILE"))?;
		let value = std::fs::read_to_string(file)?;
		ast::parse_expr(&value).map_err(|e| anyhow!("parsing {file}: {e}"))?;
		let def = ast::parse(&format!("def({name}) => const(value: {value});"))
			.map_err(|e| anyhow!("--const {name:?}: {e}"))?;
		parsed.statements.extend(def.statements);
	}

	if print_ast {
		dbg!(&parsed);
	}

	let hir = hir::parse_ast(parsed, reg)?;

	if print_hir {
		dbg!(&hir);
	}

	let mir = mir::from_hir(&hir, reg)?;

	if print_mir {
		dbg!(&mir);
	}

	let mut out = Default::default();
	match generator {
		Target::Rust => rust::generate(&mir, &mut out, reg, view.as_deref(), split.as_ref())?,
		_ if view.is_some() => bail!("--view is only supported for rust"),
		_ if split.is_some() => bail!("--crates is only supported for rust"),
		_ => GeneratorTypes::from(generator).generator()(&mir, &mut out, reg)?,
	}

	if let Some(to) = &to {
		std::fs::create_dir_all(to)?;
	}
	let mut written = Vec::new();
	for (_file, value) in out {
		let mut file = to.clone();
		if let Some(file) = &mut file {
			file.push(&_file);
			if let Some(parent) = file.parent() {
				std::fs::create_dir_all(parent)?;
			}
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
		let comment = match _file.extension().is_some_and(|e| e == "toml") {
			true => "#",
			false => "//",
		};
		let out = format!("{comment} This file is automatically generated from PDS descriptor\n{out}");

		if let Some(file) = file {
			std::fs::write(&file, out)?;
			written.push(file);
		} else {
			println!("{out}");
		}
	}
	rustfmt(&written);
	Ok(())
}

fn doc(attributes: &ast::Attributes) -> Option<String> {
	attributes
		.get(&ast::Ident::new("doc"))?
		.iter()
		.find_map(|v| match v {
			ast::Value::String { string, .. } => Some(string.clone()),
			_ => None,
		})
}

fn existing_versions(dir: &Path) -> BTreeMap<String, String> {
	let field = |line: &str, key: &str| {
		let value = line.strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
		Some(value.strip_prefix('"')?.strip_suffix('"')?.to_owned())
	};
	let Ok(entries) = std::fs::read_dir(dir) else {
		return BTreeMap::new();
	};
	entries
		.flatten()
		.filter_map(|entry| {
			let text = std::fs::read_to_string(entry.path().join("Cargo.toml")).ok()?;
			let package = text
				.lines()
				.skip_while(|l| l.trim() != "[package]")
				.skip(1)
				.take_while(|l| !l.trim_start().starts_with('['));
			let (mut name, mut version) = (None, None);
			for line in package {
				name = name.or_else(|| field(line, "name"));
				version = version.or_else(|| field(line, "version"));
			}
			Some((name?, version?))
		})
		.collect()
}

fn namespace_docs(block: &ast::Block) -> BTreeMap<String, String> {
	block
		.statements
		.iter()
		.filter_map(|stmt| {
			let ast::Value::Item(head) = stmt.items.first()? else {
				return None;
			};
			if head.name.item.as_str() != "namespace" {
				return None;
			}
			let ast::Value::Item(name) = head.args.positional().first()? else {
				return None;
			};
			Some((name.name.item.as_str().to_owned(), doc(&head.block.attributes)?))
		})
		.collect()
}

pub fn rustfmt(written: &[PathBuf]) {
	let rust: HashSet<&Path> = written
		.iter()
		.map(PathBuf::as_path)
		.filter(|f| f.extension().is_some_and(|e| e == "rs"))
		.collect();
	let declared = |f: &Path| -> bool {
		let (Some(dir), Some(name)) = (f.parent(), f.file_name()) else {
			return false;
		};
		let dir = match name.to_str() {
			Some("lib.rs") => return false,
			Some("mod.rs") => dir.parent(),
			_ => Some(dir),
		};
		dir.is_some_and(|d| ["mod.rs", "lib.rs"].iter().any(|m| rust.contains(d.join(m).as_path())))
	};
	let mut roots: Vec<&Path> = rust.iter().copied().filter(|f| !declared(f)).collect();
	if roots.is_empty() {
		return;
	}
	roots.sort();
	match Command::new("rustfmt").args(["--edition", "2021"]).args(&roots).status() {
		Ok(status) if status.success() => {}
		Ok(status) => tracing::warn!("rustfmt exited with {status}, generated code may be left unformatted"),
		Err(e) => tracing::warn!("can't run rustfmt ({e}), generated code is left unformatted"),
	}
}

pub fn rustfmt_alone(file: &Path) {
	let run = || -> io::Result<Option<Vec<u8>>> {
		let source = std::fs::read(file)?;
		let mut child = Command::new("rustfmt")
			.args(["--edition", "2021", "--emit", "stdout"])
			.current_dir(file.parent().unwrap_or(Path::new(".")))
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.spawn()?;
		child.stdin.take().expect("piped stdin").write_all(&source)?;
		let out = child.wait_with_output()?;
		Ok(out.status.success().then_some(out.stdout))
	};
	match run() {
		Ok(Some(formatted)) => {
			if let Err(e) = std::fs::write(file, formatted) {
				tracing::warn!("{}: {e}", file.display());
			}
		}
		Ok(None) => tracing::warn!("rustfmt failed on {}, left unformatted", file.display()),
		Err(e) => tracing::warn!("can't run rustfmt ({e}), {} is left unformatted", file.display()),
	}
}

const VALUE_DEF: &str = "__pds2_value";
const VALUE_NATIVE: &str =
	"@specializer const def_native(__pds2_const) { type(Value); argument(value) => Value; body; };";

fn compile_value(schema: &str, value: &str, reg: &Registry) -> Result<(mir::TypeMap, mir::Expr)> {
	let mut parsed = ast::parse(schema).map_err(|e| anyhow!("parsing the schema: {e}"))?;
	let def = ast::parse(&format!("{VALUE_NATIVE} def({VALUE_DEF}) => __pds2_const(value: {value});"))
		.map_err(|e| anyhow!("{e}"))?;
	parsed.statements.extend(def.statements);
	let types = mir::from_hir(&hir::parse_ast(parsed, reg)?, reg)?;
	let value = types
		.0
		.values()
		.find(|d| d.name.readable_name() == VALUE_DEF)
		.and_then(|d| match &d.kind {
			mir::TypeKind::Native(n) => n.ref_args.iter().find(|(n, _)| n.as_str() == "value"),
			mir::TypeKind::Alias(_) => None,
		})
		.map(|(_, e)| e.clone())
		.ok_or_else(|| anyhow!("the value was not compiled"))?;
	Ok((types, value))
}

pub fn encode(schema: &str, ty: Option<&str>, value: &str, reg: &Registry) -> Result<Vec<u8>> {
	let parsed = ast::parse_expr(value).map_err(|e| anyhow!("parsing the value: {e}"))?;
	let value = match ty {
		Some(ty) => format!("{ty}({value})"),
		None if matches!(&parsed, ast::Value::Item(i) if !i.args.is_empty()) => value.to_owned(),
		None => bail!("the value has no type, write it as `root::Type(...)` or pass --type"),
	};
	let (types, value) = compile_value(schema, &value, reg)?;
	let elab = Elaborator { types: &types, reg };
	Encoding::new(&elab).encode_root(&value)
}

pub fn decode(schema: &str, ty: &str, bytes: &[u8], reg: &Registry) -> Result<String> {
	let bytes = ast::format_value(&ast::Value::Bytes(bytes.to_vec()));
	let (types, value) = compile_value(schema, &format!("{ty}(deserialized({bytes}))"), reg)?;
	let elab = Elaborator { types: &types, reg };
	Ok(format!("{}\n", ast::format_call(ty, &source(&elab.literal(&value)?)?)))
}

pub fn format(args: FormatArgs) -> Result<()> {
	let data = std::fs::read_to_string(args.from)?;
	let parsed = ast::parse(&data)?;
	print!("{}", ast::format(&parsed));
	Ok(())
}

pub fn main(run: impl FnOnce() -> Result<()>) {
	tracing_subscriber::fmt()
		.without_time()
		.with_writer(io::stderr)
		.init();
	if let Err(err) = run() {
		let mut chain = err.chain().rev();
		eprintln!("ERROR {}", chain.next().expect("an error has a cause"));
		for ele in chain {
			eprintln!("   at {ele}");
		}
		std::process::exit(1);
	}
}
