use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail, ensure};
use pds2_core::ast::{Block, Item, Value};

use super::Dir;

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Action {
	Drop,
	Default,
	Unsupported,
	Custom(String),
	Expr(Value),
}

#[derive(Clone, Default, Debug)]
pub struct Change {
	pub up: Option<Action>,
	pub down: Option<Action>,
}

impl Change {
	pub(super) fn get(&self, dir: Dir) -> Option<&Action> {
		match dir {
			Dir::Up => self.up.as_ref(),
			Dir::Down => self.down.as_ref(),
		}
	}
}

#[derive(Clone, Default, Debug)]
pub struct Rules {
	pub path: String,
	pub own: bool,
	pub whole: Change,
	pub added: BTreeMap<String, Change>,
	pub removed: BTreeMap<String, Change>,
	pub converted: BTreeMap<String, Change>,
	pub hooked: BTreeMap<String, Change>,
	pub renamed: BTreeMap<String, String>,
	pub updated: BTreeMap<String, Rules>,
}

impl Rules {
	pub(super) fn active(&self, dir: Dir) -> bool {
		self.own
			|| self.hooked.values().any(|c| c.get(dir).is_some())
			|| self.updated.values().any(|u| u.active(dir))
	}

	fn disown(&mut self) -> Result<()> {
		ensure!(
			self.whole.up.is_none()
				&& self.whole.down.is_none()
				&& self.added.is_empty()
				&& self.removed.is_empty()
				&& self.converted.is_empty()
				&& self.renamed.is_empty(),
			"{}: rules for every step can only be `hooked` or `updated`",
			self.path
		);
		self.own = false;
		self.updated.values_mut().try_for_each(Rules::disown)
	}
}

fn first_name(item: &Item) -> Result<String> {
	match item.args.positional().first() {
		Some(Value::Item(i)) => Ok(i.name.item.as_str().to_owned()),
		_ => bail!("`{:?}` needs a name", item.name),
	}
}

fn action(item: &Item, key: &str) -> Option<Action> {
	let (_, v) = item.args.named_values().find(|(n, _)| n.as_str() == key)?;
	Some(match v {
		Value::Item(i) if i.name.item.as_str() == "drop" && i.args.is_empty() => Action::Drop,
		Value::Item(i) if i.name.item.as_str() == "default" && i.args.is_empty() => Action::Default,
		Value::Item(i) if i.name.item.as_str() == "unsupported" && i.args.is_empty() => {
			Action::Unsupported
		}
		Value::Item(i) if i.name.item.as_str() == "custom" => {
			Action::Custom(first_name(i).unwrap_or_default())
		}
		v => Action::Expr(v.clone()),
	})
}

fn change(item: &Item) -> Change {
	Change {
		up: action(item, "up"),
		down: action(item, "down"),
	}
}

fn parse_rules(item: &Item, path: String, all: &mut BTreeSet<String>) -> Result<Rules> {
	all.insert(path.clone());
	let mut r = Rules {
		path: path.clone(),
		own: true,
		whole: change(item),
		..Default::default()
	};
	for stmt in &item.block.statements {
		let Some(Value::Item(op)) = stmt.items.first() else {
			bail!("{path}: expected a rule");
		};
		let name = first_name(op)?;
		let kind = op.name.item.as_str();
		let key = format!("{path}/{kind}({name})");
		all.insert(key.clone());
		match kind {
			"added" => merge_change(r.added.entry(name).or_default(), change(op), &key)?,
			"removed" => merge_change(r.removed.entry(name).or_default(), change(op), &key)?,
			"converted" => merge_change(r.converted.entry(name).or_default(), change(op), &key)?,
			"hooked" => {
				let c = change(op);
				for a in [&c.up, &c.down].into_iter().flatten() {
					ensure!(
						matches!(a, Action::Custom(_)),
						"{key}: a hook should be custom(...)"
					);
				}
				merge_change(r.hooked.entry(name).or_default(), c, &key)?
			}
			"renamed" => {
				let (_, to) = op
					.args
					.named_values()
					.find(|(n, _)| n.as_str() == "to")
					.ok_or_else(|| anyhow!("{key}: needs `to: <new name>`"))?;
				let Value::Item(to) = to else {
					bail!("{key}: `to` should be a name");
				};
				r.renamed.insert(name, to.name.item.as_str().to_owned());
			}
			"updated" => {
				all.remove(&key);
				let nested = parse_rules(op, format!("{path}/{name}"), all)?;
				match r.updated.get_mut(&name) {
					Some(existing) => merge_rules(existing, nested)?,
					None => {
						r.updated.insert(name.clone(), nested);
					}
				}
			}
			k => bail!("{path}: unknown rule `{k}`"),
		}
	}
	Ok(r)
}

fn merge_change(into: &mut Change, from: Change, at: &str) -> Result<()> {
	for (slot, new) in [(&mut into.up, from.up), (&mut into.down, from.down)] {
		if let Some(new) = new {
			ensure!(slot.is_none(), "{at} is given twice");
			*slot = Some(new);
		}
	}
	Ok(())
}

pub(super) fn merge_rules(into: &mut Rules, from: Rules) -> Result<()> {
	into.own |= from.own;
	merge_change(&mut into.whole, from.whole, &into.path)?;
	for (map, other, kind) in [
		(&mut into.added, from.added, "added"),
		(&mut into.removed, from.removed, "removed"),
		(&mut into.converted, from.converted, "converted"),
		(&mut into.hooked, from.hooked, "hooked"),
	] {
		for (name, change) in other {
			let at = format!("{}/{kind}({name})", into.path);
			merge_change(map.entry(name).or_default(), change, &at)?;
		}
	}
	for (old, new) in from.renamed {
		ensure!(
			into.renamed.insert(old.clone(), new).is_none(),
			"{}/renamed({old}) is given twice",
			into.path
		);
	}
	for (name, nested) in from.updated {
		match into.updated.get_mut(&name) {
			Some(existing) => merge_rules(existing, nested)?,
			None => {
				into.updated.insert(name, nested);
			}
		}
	}
	Ok(())
}

pub struct Migration {
	pub rules: BTreeMap<String, Rules>,
	pub all: BTreeSet<String>,
}

pub fn parse_migration(block: &Block) -> Result<Migration> {
	fn walk(
		block: &Block,
		ns: &mut Vec<String>,
		out: &mut BTreeMap<String, Rules>,
		all: &mut BTreeSet<String>,
	) -> Result<()> {
		for stmt in &block.statements {
			let Some(Value::Item(head)) = stmt.items.first() else {
				bail!("expected `namespace` or `updated`");
			};
			let name = first_name(head)?;
			match head.name.item.as_str() {
				"namespace" => {
					ns.push(name);
					walk(&head.block, ns, out, all)?;
					ns.pop();
				}
				"updated" => {
					let full = ns
						.iter()
						.chain([&name])
						.cloned()
						.collect::<Vec<_>>()
						.join("::");
					let rules = parse_rules(head, full.clone(), all)?;
					match out.get_mut(&full) {
						Some(existing) => merge_rules(existing, rules)?,
						None => {
							out.insert(full, rules);
						}
					}
				}
				k => bail!("unexpected `{k}` in a migration"),
			}
		}
		Ok(())
	}
	let mut rules = BTreeMap::new();
	let mut all = BTreeSet::new();
	walk(block, &mut Vec::new(), &mut rules, &mut all)?;
	Ok(Migration { rules, all })
}

pub fn parse_every(block: &Block) -> Result<Migration> {
	let mut m = parse_migration(block)?;
	m.rules.values_mut().try_for_each(Rules::disown)?;
	Ok(m)
}
