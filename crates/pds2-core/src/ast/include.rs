use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail, ensure};

use super::{Block, Item, Value, parse};

pub fn load(path: &Path) -> Result<Block> {
	load_file(path, &mut Vec::new())
}

fn load_file(path: &Path, stack: &mut Vec<PathBuf>) -> Result<Block> {
	let canonical = std::fs::canonicalize(path).with_context(|| path.display().to_string())?;
	if stack.contains(&canonical) {
		let chain: Vec<String> = stack
			.iter()
			.chain([&canonical])
			.map(|p| p.display().to_string())
			.collect();
		bail!("include cycle: {}", chain.join(" -> "));
	}
	let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
	let mut block = parse(&text).map_err(|e| anyhow!("{}: {e}", path.display()))?;
	let dir = path.parent().unwrap_or(Path::new(""));
	stack.push(canonical);
	let expanded = expand(&mut block, dir, stack);
	stack.pop();
	expanded.with_context(|| path.display().to_string())?;
	Ok(block)
}

fn included_file(item: &Item) -> Result<String> {
	ensure!(item.block.statements.is_empty(), "include has no block");
	let file = match item.args.positional() {
		[Value::String { string, .. }] => string.clone(),
		_ => bail!("include takes one file name string"),
	};
	ensure!(
		item.args.named_values().next().is_none(),
		"include takes no named arguments"
	);
	Ok(file)
}

fn expand(block: &mut Block, dir: &Path, stack: &mut Vec<PathBuf>) -> Result<()> {
	for mut statement in std::mem::take(&mut block.statements) {
		let head = statement.head();
		if head.and_then(Item::head_name) == Some("include") {
			let head = head.expect("checked");
			ensure!(
				statement.attributes.is_empty() && statement.items.len() == 1,
				"include is a statement of its own"
			);
			let file = included_file(head)?;
			let inner = load_file(&dir.join(&file), stack)
				.with_context(|| format!("included from {}", dir.display()))?;
			for (key, value) in inner.attributes {
				block.attributes.entry(key).or_insert(value);
			}
			block.statements.extend(inner.statements);
			continue;
		}
		for value in &mut statement.items {
			if let Value::Item(item) = value {
				expand(&mut item.block, dir, stack)?;
			}
		}
		block.statements.push(statement);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::load;

	fn dir(name: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!("pds2-include-{name}-{}", std::process::id()));
		std::fs::create_dir_all(dir.join("sub")).unwrap();
		dir
	}

	#[test]
	fn splices_statements_and_fills_missing_attributes() {
		let d = dir("splice");
		std::fs::write(
			d.join("sub/inner.pds"),
			"@!doc \"inner\"\n@!protocol [5]\ndef(a) => u8;\ninclude(\"deeper.pds\");\n",
		)
		.unwrap();
		std::fs::write(d.join("sub/deeper.pds"), "def(b) => u8;\n").unwrap();
		std::fs::write(
			d.join("root.pds"),
			"package(p) {\n\t@!doc \"outer\"\n\tinclude(\"sub/inner.pds\");\n};\n",
		)
		.unwrap();
		let block = load(&d.join("root.pds")).unwrap();
		let text = crate::ast::format(&block);
		assert!(text.contains("@!doc \"outer\""), "{text}");
		assert!(text.contains("@!protocol [5]"), "{text}");
		assert!(!text.contains("inner\""), "{text}");
		assert!(text.contains("def(a)") && text.contains("def(b)"), "{text}");
		assert!(!text.contains("include"), "{text}");
	}

	#[test]
	fn rejects_cycles() {
		let d = dir("cycle");
		std::fs::write(d.join("a.pds"), "include(\"b.pds\");\n").unwrap();
		std::fs::write(d.join("b.pds"), "include(\"a.pds\");\n").unwrap();
		let err = format!("{:#}", load(&d.join("a.pds")).unwrap_err());
		assert!(err.contains("include cycle"), "{err}");
	}
}
