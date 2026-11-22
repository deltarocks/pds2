use std::any::Any;
use std::collections::BTreeMap;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use genco::lang::rust;

use crate::ast::{Attributes, Block, Ident, Item, ParseValue, Value};
use crate::generator::GeneratorOutput;
use crate::generator::rust::DefinitionRustCtx;
use crate::mir;
use crate::specializer::Registry;

pub const BUILTIN_STATEMENTS: &[&str] = &["namespace", "def", "def_native", "fn"];

const MAX_DEPTH: usize = 32;

pub struct LowerCtx<'a> {
	pub namespace: &'a [Ident],
	pub reg: &'a Registry,
}

pub struct Lowered {
	pub statements: Vec<crate::ast::Statement>,
	pub data: Box<dyn Any>,
}

pub struct DefinitionUse {
	pub definition: Ident,
	pub namespace: Vec<Ident>,
	pub data: Box<dyn Any>,
}

impl DefinitionUse {
	pub fn data<T: 'static>(&self) -> Result<&T> {
		self.data
			.downcast_ref()
			.ok_or_else(|| anyhow!("`{}` carries data of another definition", self.definition))
	}
}

pub struct ExtraModule {
	pub namespace: Vec<Ident>,
	pub tokens: rust::Tokens,
}

pub trait Definition {
	fn lower(
		&self,
		attributes: Attributes,
		item: Item,
		chain: Vec<Value>,
		ctx: &LowerCtx,
	) -> Result<Lowered>;
	fn generate_rust(
		&self,
		uses: &[&DefinitionUse],
		ctx: &DefinitionRustCtx,
	) -> Result<Vec<ExtraModule>>;
	fn generate_typescript(
		&self,
		_uses: &[&DefinitionUse],
		_types: &mir::TypeMap,
		_output: &mut GeneratorOutput,
	) -> Result<()> {
		bail!("this definition has no TypeScript generator")
	}
}

pub fn lower(mut block: Block, reg: &Registry) -> Result<(Block, Vec<DefinitionUse>)> {
	let mut uses = Vec::new();
	lower_block(&mut block, reg, &mut Vec::new(), &mut uses, 0)?;
	Ok((block, uses))
}

fn lower_block(
	block: &mut Block,
	reg: &Registry,
	namespace: &mut Vec<Ident>,
	uses: &mut Vec<DefinitionUse>,
	depth: usize,
) -> Result<()> {
	ensure!(depth <= MAX_DEPTH, "definition expansion too deep");
	for mut statement in std::mem::take(&mut block.statements) {
		let Some(head) = statement.head().and_then(Item::head_name).map(Ident::new) else {
			block.statements.push(statement);
			continue;
		};
		match head.as_str() {
			"namespace" => {
				if let Some(Value::Item(item)) = statement.items.first_mut() {
					let name = item.args.positional().first().cloned().map(Ident::parse);
					if let Some(Ok(name)) = name {
						namespace.push(name.clone());
						let lowered = lower_block(&mut item.block, reg, namespace, uses, depth);
						namespace.pop();
						lowered.with_context(|| format!("namespace {name:?}"))?;
					}
				}
				block.statements.push(statement);
			}
			"def" | "def_native" | "fn" => block.statements.push(statement),
			name => {
				let Some(definition) = reg.definition(name) else {
					let mut known: Vec<&str> = reg.definition_names().collect();
					known.sort_unstable();
					let known = if known.is_empty() {
						"none".to_owned()
					} else {
						known.join(", ")
					};
					bail!("unknown top level statement `{name}` (registered definitions: {known})");
				};
				let (attributes, item, chain) = statement.pop_item()?;
				let label = match item.args.positional().first().and_then(Value::item) {
					Some(arg) => format!("{name}({})", arg.name.item),
					None => name.to_owned(),
				};
				let lowered = definition
					.lower(attributes, item, chain, &LowerCtx { namespace, reg })
					.with_context(|| format!("in `{label}`"))?;
				uses.push(DefinitionUse {
					definition: head.clone(),
					namespace: namespace.clone(),
					data: lowered.data,
				});
				let mut inner = Block {
					statements: lowered.statements,
					..Default::default()
				};
				lower_block(&mut inner, reg, namespace, uses, depth + 1)
					.with_context(|| format!("expanding `{label}`"))?;
				block.statements.extend(inner.statements);
			}
		}
	}
	Ok(())
}

pub fn group(uses: &[DefinitionUse]) -> BTreeMap<&str, Vec<&DefinitionUse>> {
	let mut out: BTreeMap<&str, Vec<&DefinitionUse>> = BTreeMap::new();
	for u in uses {
		out.entry(u.definition.as_str()).or_default().push(u);
	}
	out
}

pub fn generate_typescript(
	types: &mir::TypeMap,
	uses: &[DefinitionUse],
	reg: &Registry,
	output: &mut GeneratorOutput,
) -> Result<()> {
	for (name, uses) in group(uses) {
		reg.definition(name)
			.ok_or_else(|| anyhow!("definition {name:?} is not registered"))?
			.generate_typescript(&uses, types, output)
			.with_context(|| format!("generating `{name}` definitions"))?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use anyhow::Result;
	use genco::prelude::*;

	use super::{Definition, DefinitionUse, ExtraModule, LowerCtx, Lowered};
	use crate::ast::{self, Attributes, Ident, Item, Value};
	use crate::generator::rust::DefinitionRustCtx;
	use crate::specializer::Registry;
	use crate::{hir, mir};

	struct Pair;
	impl Definition for Pair {
		fn lower(
			&self,
			_attributes: Attributes,
			item: Item,
			chain: Vec<Value>,
			_ctx: &LowerCtx,
		) -> Result<Lowered> {
			let name: Ident = item.args.arg(0, "name")?;
			item.args.verify_unused()?;
			let mut statements = Vec::new();
			for suffix in ["a", "b"] {
				let def = Item::simple_builder("def")
					.argsb(
						ast::ItemArgs::builder()
							.arg(Value::new_simple_item(format!("{name}_{suffix}").as_str())),
					)
					.block(ast::Block {
						statements: vec![ast::Statement::simple(vec![Value::new_simple_item(
							"export",
						)])],
						..Default::default()
					})
					.build();
				statements.push(ast::Statement::simple(
					std::iter::once(Value::Item(def))
						.chain(chain.iter().cloned())
						.collect(),
				));
			}
			Ok(Lowered {
				statements,
				data: Box::new(name),
			})
		}
		fn generate_rust(
			&self,
			uses: &[&DefinitionUse],
			_ctx: &DefinitionRustCtx,
		) -> Result<Vec<ExtraModule>> {
			uses.iter()
				.map(|u| {
					let name: &Ident = u.data()?;
					Ok(ExtraModule {
						namespace: u.namespace.clone(),
						tokens: quote!(pub const $(name.as_str().to_uppercase()): u8 = 1;),
					})
				})
				.collect()
		}
	}

	struct Nest;
	impl Definition for Nest {
		fn lower(
			&self,
			_: Attributes,
			item: Item,
			chain: Vec<Value>,
			_: &LowerCtx,
		) -> Result<Lowered> {
			Ok(Lowered {
				statements: vec![ast::Statement::simple(
					std::iter::once(Value::Item(item)).chain(chain).collect(),
				)],
				data: Box::new(()),
			})
		}
		fn generate_rust(
			&self,
			_: &[&DefinitionUse],
			_: &DefinitionRustCtx,
		) -> Result<Vec<ExtraModule>> {
			Ok(vec![])
		}
	}

	const NATIVES: &str = "@scalar u8 @encoding be def_native(u8);";

	fn registry() -> Registry {
		let mut reg = Registry::default();
		reg.register_definition("pair", Pair);
		reg
	}

	#[test]
	fn lowers_inside_namespace() {
		let reg = registry();
		let block = ast::parse(&format!("{NATIVES} namespace(ns) {{ pair(x) => u8; }};")).unwrap();
		let (lowered, uses) = super::lower(block, &reg).unwrap();
		assert_eq!(uses.len(), 1);
		assert_eq!(uses[0].namespace, vec![Ident::new("ns")]);
		assert_eq!(uses[0].data::<Ident>().unwrap(), &Ident::new("x"));
		let types = mir::from_hir(&hir::parse_ast(lowered, &reg).unwrap(), &reg).unwrap();
		let names: Vec<String> = types.0.values().map(|d| format!("{:?}", d.name)).collect();
		assert!(names.contains(&"root::ns::x_a".to_owned()), "{names:?}");
		assert!(names.contains(&"root::ns::x_b".to_owned()), "{names:?}");
	}

	#[test]
	fn parse_ast_accepts_definitions() {
		let reg = registry();
		let block = ast::parse(&format!("{NATIVES} pair(y) => u8;")).unwrap();
		hir::parse_ast(block, &reg).unwrap();
	}

	#[test]
	fn unknown_statement() {
		let reg = registry();
		let block = ast::parse("bogus(x);").unwrap();
		let err = format!("{:#}", super::lower(block, &reg).err().unwrap());
		assert!(
			err.contains("unknown top level statement `bogus` (registered definitions: pair)"),
			"{err}"
		);
	}

	#[test]
	#[should_panic(expected = "shadow")]
	fn builtin_name_is_reserved() {
		Registry::default().register_definition("def", Pair);
	}

	#[test]
	fn expansion_depth_is_capped() {
		let mut reg = Registry::default();
		reg.register_definition("nest", Nest);
		let block = ast::parse("nest(x);").unwrap();
		let err = format!("{:#}", super::lower(block, &reg).err().unwrap());
		assert!(err.contains("definition expansion too deep"), "{err}");
	}

	#[test]
	fn extra_module_lands_in_namespace_mod() {
		let reg = registry();
		let block = ast::parse(&format!("{NATIVES} namespace(ns) {{ pair(x) => u8; }};")).unwrap();
		let (lowered, uses) = super::lower(block, &reg).unwrap();
		let types = mir::from_hir(&hir::parse_ast(lowered, &reg).unwrap(), &reg).unwrap();
		let mut out = Default::default();
		crate::generator::rust::generate(&types, &mut out, &reg, None, None, &uses).unwrap();
		let module = &out[&std::path::PathBuf::from("ns/mod.rs")];
		assert!(module.contains("pub const X: u8 = 1;"), "{module}");
	}

	#[test]
	fn extra_module_needs_types_in_namespace() {
		let reg = registry();
		let block = ast::parse(&format!("{NATIVES} pair(x) => u8;")).unwrap();
		let (lowered, mut uses) = super::lower(block, &reg).unwrap();
		uses[0].namespace = vec![Ident::new("missing")];
		let types = mir::from_hir(&hir::parse_ast(lowered, &reg).unwrap(), &reg).unwrap();
		let mut out = Default::default();
		let err = format!(
			"{:#}",
			crate::generator::rust::generate(&types, &mut out, &reg, None, None, &uses)
				.err()
				.unwrap()
		);
		assert!(err.contains("has no generated module"), "{err}");
	}
}
