use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::rust;
use genco::prelude::*;
use pds2_core::ast::{Block, FieldRefSegment, Item, Value};
use pds2_core::generator::common::scalar_of;
use pds2_core::generator::recursion::{feedback_vertices, Graph};
use pds2_core::generator::naming::{Case, Scope};
use pds2_core::generator::rust::RustNames;
use pds2_core::hir::TypeId;
use pds2_core::mir::{self, Expr, FieldItem, TypeKind, TypeMap};
use pds2_core::specializer::expr::Elaborator;
use pds2_core::specializer::{specialization_use, Registry, VALUE};
use pds2_specializer_protodef::{container, union};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Dir {
	Up,
	Down,
}

impl Dir {
	fn name(self) -> &'static str {
		match self {
			Dir::Up => "up",
			Dir::Down => "down",
		}
	}
}

#[derive(Clone, Debug)]
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
	fn get(&self, dir: Dir) -> Option<&Action> {
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
	fn active(&self, dir: Dir) -> bool {
		self.own || self.hooked.values().any(|c| c.get(dir).is_some()) || self.updated.values().any(|u| u.active(dir))
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
					ensure!(matches!(a, Action::Custom(_)), "{key}: a hook should be custom(...)");
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

fn merge_rules(into: &mut Rules, from: Rules) -> Result<()> {
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
		ensure!(into.renamed.insert(old.clone(), new).is_none(), "{}/renamed({old}) is given twice", into.path);
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
					let full = ns.iter().chain([&name]).cloned().collect::<Vec<_>>().join("::");
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

#[derive(Clone)]
enum Conv {
	Identity,
	Call(String),
}

struct ConvFn {
	name: String,
	from: rust::Tokens,
	ret: rust::Tokens,
	body: rust::Tokens,
}
impl ConvFn {
	fn emit(&self, guarded: bool) -> rust::Tokens {
		let (name, from, ret, body) = (&self.name, &self.from, &self.ret, &self.body);
		quote! {
			pub fn $name(v: $from, cx: &mut crate::Session) -> crate::pds::convert::Result<$ret> {
				$(if guarded => crate::pds::stack::check()?;)
				$body
			}
		}
	}
}

struct Gen<'a> {
	types: &'a TypeMap,
	names: RustNames<'a>,
	elab: Elaborator<'a>,
	base: BTreeMap<String, String>,
	alias_base: BTreeMap<TypeId, String>,
	fns: RefCell<BTreeMap<(usize, Dir, TypeId, TypeId, bool, Option<String>), String>>,
	code: RefCell<Vec<ConvFn>>,
	calls: RefCell<Graph<String>>,
	current: RefCell<Vec<String>>,
	errors: RefCell<BTreeSet<String>>,
	used: RefCell<BTreeSet<String>>,
	dirty: RefCell<BTreeMap<(usize, Dir, TypeId), bool>>,
	counter: Cell<usize>,
}

fn full_name(types: &TypeMap, id: TypeId) -> String {
	let p = &types[id].name;
	p.path
		.iter()
		.chain([&p.item])
		.map(|i| i.as_str())
		.collect::<Vec<_>>()
		.join("::")
}

struct Step<'s> {
	index: usize,
	name: String,
	label: String,
	rules: &'s BTreeMap<String, Rules>,
}

fn kind_of(types: &TypeMap, id: TypeId) -> Option<&str> {
	match &types[id].kind {
		TypeKind::Native(n) => n.specializer.as_ref().map(|(s, _)| s.as_str()),
		TypeKind::Alias(_) => None,
	}
}

fn native(types: &TypeMap, id: TypeId) -> Option<&mir::NativeType> {
	match &types[id].kind {
		TypeKind::Native(n) => Some(n),
		TypeKind::Alias(_) => None,
	}
}

fn stored_fields(types: &TypeMap, id: TypeId) -> Option<Vec<(String, TypeId)>> {
	let n = native(types, id)?;
	match kind_of(types, id)? {
		"container" => Some(
			container::view(n)?
				.into_iter()
				.filter(|f| f.load_from.is_none())
				.map(|f| (f.name.as_str().to_owned(), f.ty.target))
				.collect(),
		),
		"bitfield" | "bitflags" => Some(
			n.fields
				.iter()
				.map(|(name, t)| (name.as_str().to_owned(), t.target))
				.collect(),
		),
		_ => None,
	}
}

impl<'a> Gen<'a> {
	fn resolve(&self, id: TypeId) -> TypeId {
		self.elab.resolve(id)
	}

	fn children(&self, id: TypeId) -> Vec<TypeId> {
		if let Some(fields) = stored_fields(self.types, id) {
			return fields.into_iter().map(|(_, t)| t).collect();
		}
		let Some(n) = native(self.types, id) else {
			return vec![];
		};
		if let Some(u) = union::view(n) {
			return u
				.arms
				.iter()
				.map(|(_, _, t)| t.target)
				.chain(u.default.map(|(_, t)| t.target))
				.collect();
		}
		specialization_use(n, VALUE).map(|t| t.target).into_iter().collect()
	}

	fn base_name(&self, id: TypeId) -> Option<&String> {
		let resolved = self.resolve(id);
		self.base
			.get(&full_name(self.types, id))
			.or_else(|| self.base.get(&full_name(self.types, resolved)))
			.or_else(|| self.alias_base.get(&resolved))
	}

	fn dirty(&self, step: &Step, dir: Dir, id: TypeId) -> bool {
		let id = self.resolve(id);
		if let Some(d) = self.dirty.borrow().get(&(step.index, dir, id)) {
			return *d;
		}
		self.dirty.borrow_mut().insert((step.index, dir, id), false);
		let own = self.base_name(id).and_then(|b| step.rules.get(b)).is_some_and(|r| r.active(dir));
		let d = own || self.children(id).into_iter().any(|c| self.dirty(step, dir, c));
		self.dirty.borrow_mut().insert((step.index, dir, id), d);
		d
	}

	fn leaf(&self, id: TypeId) -> bool {
		let id = self.resolve(id);
		match kind_of(self.types, id) {
			Some("container" | "bitfield" | "bitflags" | "union" | "registryEntryHolder" | "registryEntryHolderSet") => false,
			Some(_) => self.children(id).into_iter().all(|c| self.leaf(c)),
			None => true,
		}
	}

	fn hook_tree(&self, dir: Dir, versions: (usize, usize), id: TypeId, r: &Rules, place: rust::Tokens, depth: usize, out: &mut Vec<rust::Tokens>) {
		let id = self.resolve(id);
		let p = format!("__h{depth}");
		let visit = |name: &str, ty: TypeId, value: rust::Tokens, body: &mut Vec<rust::Tokens>| {
			if let Some(Action::Custom(f)) = r.hooked.get(name).and_then(|c| c.get(dir)) {
				if self.leaf(ty) {
					let (f, (a, b)) = (self.custom(f), versions);
					body.push(quote!($f(&mut $(&value), $a, $b, cx)?;));
					self.used(format!("{}/hooked({name})", r.path));
				}
			}
			if let Some(nested) = r.updated.get(name) {
				self.used(nested.path.clone());
				self.hook_tree(dir, versions, ty, nested, value, depth + 1, body);
			}
		};
		if let Some(fields) = stored_fields(self.types, id) {
			for (name, ty) in fields {
				let ident = pds2_core::ast::Ident::new(&name);
				let value = if self.names.boxed(id, &ident) {
					quote!((*$(&place).$(self.names.field(id, &name))))
				} else {
					quote!($(&place).$(self.names.field(id, &name)))
				};
				visit(&name, ty, value, out);
			}
			return;
		}
		let Some(n) = native(self.types, id) else { return };
		let ty = self.names.ty(id);
		let inner = |boxed: bool| if boxed { quote!((**$(&p))) } else { quote!((*$(&p))) };
		if let Some(u) = union::view(n) {
			let mut arms = Vec::new();
			let void = |t: TypeId| self.types.is_void(self.resolve(t));
			for (_, name, t) in u.arms.iter().filter(|(_, _, t)| !void(t.target)) {
				let mut body = Vec::new();
				visit(name.as_str(), t.target, inner(self.names.boxed(id, name)), &mut body);
				if !body.is_empty() {
					arms.push(quote!($(&ty)::$(self.names.variant(id, name.as_str()))($(&p)) => { $(for b in body => $b) }));
				}
			}
			if let Some((name, t)) = u.default.filter(|(_, t)| !void(t.target)) {
				let mut body = Vec::new();
				visit("default", t.target, inner(self.names.boxed(id, name)), &mut body);
				if !body.is_empty() {
					arms.push(quote!($(&ty)::$(self.names.variant(id, name.as_str()))(_, $(&p)) => { $(for b in body => $b) }));
				}
			}
			if !arms.is_empty() {
				out.push(quote!(match &mut $(&place) { $(for a in arms => $a) _ => {} }));
			}
			return;
		}
		let slot = pds2_core::ast::Ident::new(VALUE);
		let mut body = Vec::new();
		match kind_of(self.types, id) {
			Some("array" | "terminated" | "entityMetadataLoop" | "topBitSetTerminatedArray") => {
				let Some(e) = self.element(id) else { return };
				visit("items", e, quote!((*$(&p))), &mut body);
				if !body.is_empty() {
					out.push(quote!(for $(&p) in $(&place).iter_mut() { $(for b in body => $b) }));
				}
			}
			Some("option") => {
				let Some(e) = self.element(id) else { return };
				visit("value", e, inner(self.names.boxed(id, &slot)), &mut body);
				if !body.is_empty() {
					out.push(quote!(if let Some($(&p)) = &mut $(&place) { $(for b in body => $b) }));
				}
			}
			Some("registryEntryHolder") => {
				let Some((name, t)) = n.fields.first() else { return };
				visit(name.as_str(), t.target, inner(self.names.boxed(id, name)), &mut body);
				if !body.is_empty() {
					out.push(quote!(if let $(&ty)::Inline($(&p)) = &mut $(&place) { $(for b in body => $b) }));
				}
			}
			_ => {}
		}
	}

	fn alias_names(&self, id: TypeId, path: Option<String>, out: &mut BTreeMap<String, TypeId>) {
		let id = self.resolve(id);
		let name = match self.base.get(&full_name(self.types, id)) {
			Some(base) => base.replace("::", "_"),
			None => match path {
				Some(p) if !matches!(&self.types[id].kind, TypeKind::Native(n) if n.specializer.is_none()) => p,
				_ => return,
			},
		};
		if out.values().any(|v| *v == id) || out.contains_key(&name) {
			return;
		}
		out.insert(name.clone(), id);
		let TypeKind::Native(n) = &self.types[id].kind else { return };
		for (field, t) in &n.fields {
			self.alias_names(t.target, Some(format!("{name}__{}", field.as_str())), out);
		}
		if let Some(t) = specialization_use(n, VALUE) {
			self.alias_names(t.target, Some(format!("{name}__items")), out);
		}
	}

	fn error(&self, msg: String) {
		self.errors.borrow_mut().insert(msg);
	}

	fn used(&self, key: String) {
		self.used.borrow_mut().insert(key);
	}

	fn numeric(&self, id: TypeId) -> bool {
		scalar_of(self.types, id).is_some_and(|s| s.is_number() || s == pds2_core::specializer::Scalar::Bool)
	}

	fn fresh(&self, step: &Step, dir: Dir, to: TypeId) -> String {
		let n = self.counter.get();
		self.counter.set(n + 1);
		format!("{}_{}_{}_{n}", dir.name(), step.label, self.names.flat_name(to))
	}

	fn apply(&self, conv: &Conv, value: rust::Tokens) -> rust::Tokens {
		match conv {
			Conv::Identity => value,
			Conv::Call(f) => quote!($f($value, cx)?),
		}
	}

	fn cast(&self, value: rust::Tokens, to: TypeId) -> rust::Tokens {
		if scalar_of(self.types, to) == Some(pds2_core::specializer::Scalar::Bool) {
			quote!(($value != 0))
		} else {
			quote!(($value as $(self.names.ty(to))))
		}
	}

	fn unsupported(&self, at: &str) -> rust::Tokens {
		quote!(return crate::pds::convert::unconvertible($(quoted(at))))
	}

	fn custom(&self, name: &str) -> rust::Tokens {
		quote!(crate::custom::$name)
	}

	fn expr(&self, v: &Value, src: TypeId) -> Result<Expr> {
		Ok(match v {
			Value::Int(n) => Expr::Const(mir::Value::Signed(*n)),
			Value::Float(f) => Expr::Const(mir::Value::Float(f.0)),
			Value::String { string, .. } => Expr::Const(mir::Value::String(string.clone())),
			Value::Binary(op, a, b) => {
				Expr::Binary(*op, Box::new(self.expr(a, src)?), Box::new(self.expr(b, src)?))
			}
			Value::FieldRef(r) => {
				ensure!(r.up == 0, "rule expressions read the converted value with `./field`");
				let mut cur = self.resolve(src);
				let mut items = Vec::new();
				for seg in &r.path.0 {
					let FieldRefSegment::Down { to, .. } = seg else {
						bail!("rule expressions can only read stored fields");
					};
					let fields = stored_fields(self.types, cur)
						.ok_or_else(|| anyhow!("{} has no fields", full_name(self.types, cur)))?;
					let (_, ty) = fields
						.iter()
						.find(|(n, _)| n == to.as_str())
						.ok_or_else(|| anyhow!("{} has no field {to:?}", full_name(self.types, cur)))?;
					items.push(FieldItem {
						from: Cell::new(cur),
						segment: seg.clone(),
						to: Cell::new(*ty),
					});
					cur = self.resolve(*ty);
				}
				Expr::Field(mir::Field(items))
			}
			Value::List(items) => Expr::List(items.iter().map(|i| self.expr(i, src)).collect::<Result<_>>()?),
			Value::Record(fields) => Expr::Record(
				fields
					.iter()
					.map(|(n, v)| Ok((n.clone(), self.expr(v, src)?)))
					.collect::<Result<_>>()?,
			),
			Value::Item(item) if item.args.positional().is_empty() && !item.args.is_empty() => Expr::Record(
				item.args
					.named_values()
					.map(|(n, v)| Ok((n.clone(), self.expr(v, src)?)))
					.collect::<Result<_>>()?,
			),
			Value::Item(item) if item.args.positional().len() == 1 => {
				let name = item.name.readable_name();
				let (id, _) = self
					.types
					.0
					.iter()
					.find(|(_, d)| d.name.readable_name() == name)
					.ok_or_else(|| anyhow!("unknown type {name}"))?;
				Expr::Cast(Box::new(self.expr(&item.args.positional()[0], src)?), *id)
			}
			Value::Item(item) if item.args.is_empty() => match item.name.item.as_str() {
				"true" => Expr::Const(mir::Value::Bool(true)),
				"self" => Expr::Var(pds2_core::ast::Ident::new("_0"), self.resolve(src)),
				"false" => Expr::Const(mir::Value::Bool(false)),
				_ => Expr::Const(mir::Value::UnionDescriminator(item.name.item.clone())),
			},
			v => bail!("unsupported rule expression {v:?}"),
		})
	}

	fn field_value(
		&self,
		action: &Action,
		src: TypeId,
		to_field: TypeId,
		at: &str,
	) -> Option<rust::Tokens> {
		match action {
			Action::Default => {
				self.error(format!("{at}: `default` only applies to union arms"));
				None
			}
			Action::Drop => Some(quote!(return Err(crate::pds::convert::Error::Dropped))),
			Action::Unsupported => Some(self.unsupported(at)),
			Action::Custom(f) => {
				let f = self.custom(f);
				Some(quote!($f(_0, cx)?))
			}
			Action::Expr(v) => {
				let lowered = self
					.expr(v, src)
					.and_then(|e| self.elab.coerce(e, to_field))
					.and_then(|e| self.names.lower(&e));
				match lowered {
					Ok(t) => Some(t),
					Err(e) => {
						self.error(format!("{at}: {e:#}"));
						None
					}
				}
			}
		}
	}

	fn convert(
		&self,
		step: &Step,
		dir: Dir,
		from: TypeId,
		to: TypeId,
		rules: Option<&Rules>,
		root: bool,
		at: &str,
	) -> Conv {
		let (bf, bt) = (self.base_name(from), self.base_name(to));
		let (from, to) = (self.resolve(from), self.resolve(to));
		if let Some(r) = rules {
			self.used(r.path.clone());
		}
		let rules = rules.filter(|r| r.own);
		let passed = rules.is_some();
		if from == to && !root && rules.is_none() && !self.dirty(step, dir, from) {
			return Conv::Identity;
		}
		let same = bf.is_some() && bf == bt;
		let at = match (same, rules) {
			(true, None) => format!("{}: {}", step.name, bt.expect("same base")),
			_ => at.to_owned(),
		};
		let at = at.as_str();
		if !root && from != to && self.numeric(from) && self.numeric(to) {
			self.error(format!(
				"{at}: {} became {}, mention it with `converted`",
				full_name(self.types, from),
				full_name(self.types, to)
			));
			return Conv::Identity;
		}
		let rules = match rules {
			Some(r) => Some(r),
			None if same => bt
				.and_then(|b| step.rules.get(b))
				.inspect(|r| self.used(r.path.clone()))
				.filter(|r| r.active(dir)),
			None => None,
		};
		if from == to && !root && rules.is_none() && !self.dirty(step, dir, from) {
			return Conv::Identity;
		}
		let key = (step.index, dir, from, to, root, rules.map(|r| r.path.clone()));
		if let Some(name) = self.fns.borrow().get(&key) {
			self.call(name);
			return Conv::Call(name.clone());
		}
		let name = self.fresh(step, dir, to);
		self.call(&name);
		self.fns.borrow_mut().insert(key, name.clone());
		self.current.borrow_mut().push(name.clone());
		let (from_ty, to_ty) = (self.names.ty(from), self.names.ty(to));
		let ret = if root { quote!(Vec<$(&to_ty)>) } else { to_ty.clone() };
		let mut hooks = Vec::new();
		if let Some(r) = rules.filter(|_| !passed) {
			let versions = match dir {
				Dir::Up => (step.index, step.index + 1),
				Dir::Down => (step.index + 1, step.index),
			};
			self.hook_tree(dir, versions, from, r, quote!(v), 0, &mut hooks);
		}
		let body = self.body(step, dir, from, to, rules, root, at);
		let body = body.unwrap_or_else(|| quote!(unreachable!()));
		let body = if hooks.is_empty() {
			body
		} else {
			quote! {
				let mut v = v;
				$(for h in hooks => $h)
				$body
			}
		};
		self.current.borrow_mut().pop();
		self.code.borrow_mut().push(ConvFn {
			name: name.clone(),
			from: from_ty,
			ret,
			body,
		});
		Conv::Call(name)
	}

	fn call(&self, callee: &str) {
		if let Some(caller) = self.current.borrow().last() {
			self.calls.borrow_mut().entry(caller.clone()).or_default().insert(callee.to_owned());
		}
	}

	fn body(
		&self,
		step: &Step,
		dir: Dir,
		from: TypeId,
		to: TypeId,
		rules: Option<&Rules>,
		root: bool,
		at: &str,
	) -> Option<rust::Tokens> {
		if let Some(action) = rules.and_then(|r| r.whole.get(dir)) {
			return match action {
				Action::Custom(f) => {
					let f = self.custom(f);
					Some(quote!($f(v, cx)))
				}
				Action::Drop if root => Some(quote!(Ok(vec![]))),
				Action::Drop => Some(quote!(Err(crate::pds::convert::Error::Dropped))),
				Action::Unsupported => Some(self.unsupported(at)),
				Action::Expr(_) if !root => {
					let value = self.field_value(action, from, to, at)?;
					Some(quote! {
						let _0 = &v;
						Ok($value)
					})
				}
				_ => {
					self.error(format!("{at}: `{}` of a whole packet should be drop or custom(...)", dir.name()));
					None
				}
			};
		}
		let wrap = |t: rust::Tokens| if root { quote!(Ok(vec![$t])) } else { quote!(Ok($t)) };
		let (kf, kt) = (kind_of(self.types, from), kind_of(self.types, to));
		if from == to && rules.is_none() && !self.dirty(step, dir, from) {
			return Some(wrap(quote!(v)));
		}
		match (kf, kt) {
			(Some("container" | "bitfield" | "bitflags"), Some(k2)) if kf == Some(k2) => {
				self.record(step, dir, from, to, rules, at).map(wrap)
			}
			(Some("union"), Some("union")) => self.union(step, dir, from, to, rules, root, at),
			(Some(a), Some(b))
				if a == b
					&& matches!(
						a,
						"array" | "terminated" | "entityMetadataLoop" | "topBitSetTerminatedArray"
					) =>
			{
				let (ef, et) = (self.element(from)?, self.element(to)?);
				let (rf, rt) = (self.resolve(ef), self.resolve(et));
				if rf != rt && self.numeric(rf) && self.numeric(rt) && rules.is_some_and(|r| r.converted.contains_key("items")) {
					self.used(format!("{}/converted(items)", rules.expect("checked").path));
					let cast = self.cast(quote!(x), rt);
					let items = quote!(v.into_iter().map(|x| $cast).collect::<Vec<_>>());
					return Some(wrap(items));
				}
				match self.convert(step, dir, ef, et, rules.and_then(|r| r.updated.get("items")), false, &format!("{at}/items")) {
					Conv::Identity => Some(wrap(quote!(v))),
					Conv::Call(f) => {
						let items = quote! {
							v.into_iter()
								.filter_map(|x| match $f(x, cx) {
									Err(crate::pds::convert::Error::Dropped) => None,
									r => Some(r),
								})
								.collect::<crate::pds::convert::Result<Vec<_>>>()
						};
						Some(if root { quote!(Ok(vec![$items?])) } else { items })
					}
				}
			}
			(Some("option"), Some("option")) => {
				let (ef, et) = (self.element(from)?, self.element(to)?);
				let slot = pds2_core::ast::Ident::new(VALUE);
				let unbox = if self.names.boxed(from, &slot) { quote!(*x) } else { quote!(x) };
				let rebox = |t: rust::Tokens| if self.names.boxed(to, &slot) { quote!(Box::new($t)) } else { t };
				let conv = self.convert(step, dir, ef, et, rules.and_then(|r| r.updated.get("value")), false, &format!("{at}/value"));
				let inner = rebox(self.apply(&conv, unbox));
				let mapped = quote! {
					match v.map(|x| -> crate::pds::convert::Result<_> { Ok($inner) }).transpose() {
						Err(crate::pds::convert::Error::Dropped) => None,
						r => r?,
					}
				};
				Some(wrap(mapped))
			}
			(Some("registryEntryHolder"), Some("registryEntryHolder")) => {
				let (fname, fty) = native(self.types, from)?.fields.first()?;
				let (tname, tty) = native(self.types, to)?.fields.first()?;
				let unbox = if self.names.boxed(from, fname) { quote!(*p) } else { quote!(p) };
				let conv = self.convert(step, dir, fty.target, tty.target, rules.and_then(|r| r.updated.get(fname.as_str())), false, &format!("{at}/{}", fname.as_str()));
				let value = self.apply(&conv, unbox);
				let value = if self.names.boxed(to, tname) { quote!(Box::new($value)) } else { value };
				let (fm, tm) = (self.names.ty(from), self.names.ty(to));
				Some(wrap(quote!(match v { $(&fm)::Id(i) => $(&tm)::Id(i), $(&fm)::Inline(p) => $(&tm)::Inline($value) })))
			}
			(Some("registryEntryHolderSet"), Some("registryEntryHolderSet")) => {
				let (fm, tm) = (self.names.ty(from), self.names.ty(to));
				Some(wrap(quote!(match v { $(&fm)::Named(n) => $(&tm)::Named(n), $(&fm)::Ids(i) => $(&tm)::Ids(i) })))
			}
			_ if from == to && kf.is_some_and(|k| matches!(k, "buffer" | "pstring")) => Some(wrap(quote!(v))),
			(Some(a), Some(b)) if a == b && matches!(a, "buffer" | "pstring") => Some(wrap(quote!(v))),
			_ if self.numeric(from) && self.numeric(to) => {
				self.error(format!(
					"{at}: {} became {}, mention it with `converted`",
					full_name(self.types, from),
					full_name(self.types, to)
				));
				None
			}
			_ => {
				self.error(format!(
					"{at}: {} and {} can't be converted automatically, add a rule",
					full_name(self.types, from),
					full_name(self.types, to)
				));
				None
			}
		}
	}

	fn element(&self, id: TypeId) -> Option<TypeId> {
		Some(specialization_use(native(self.types, id)?, VALUE)?.target)
	}

	fn record(
		&self,
		step: &Step,
		dir: Dir,
		from: TypeId,
		to: TypeId,
		rules: Option<&Rules>,
		at: &str,
	) -> Option<rust::Tokens> {
		let src = stored_fields(self.types, from)?;
		let dst = stored_fields(self.types, to)?;
		let empty = Rules::default();
		let r = rules.unwrap_or(&empty);
		let rule_key = |kind: &str, name: &str| format!("{}/{kind}({name})", r.path);
		let source_name = |d: &str| -> String {
			match dir {
				Dir::Up => r
					.renamed
					.iter()
					.find(|(_, new)| *new == d)
					.map(|(old, _)| old.clone())
					.unwrap_or_else(|| d.to_owned()),
				Dir::Down => r.renamed.get(d).cloned().unwrap_or_else(|| d.to_owned()),
			}
		};
		let mut computed = Vec::new();
		let mut moved = Vec::new();
		let mut fields = Vec::new();
		let mut consumed = BTreeSet::new();
		for (d, dty) in &dst {
			let s = source_name(d);
			let newer = match dir {
				Dir::Up => d.clone(),
				Dir::Down => s.clone(),
			};
			if s != *d {
				let old = if dir == Dir::Up { &s } else { d };
				self.used(rule_key("renamed", old));
			}
			let local = format!("__f_{d}");
			let field_at = format!("{at}/{d}");
			if let Some((_, sty)) = src.iter().find(|(n, _)| *n == s) {
				consumed.insert(s.clone());
				let rule = r.converted.get(&newer);
				if rule.is_some() {
					self.used(rule_key("converted", &newer));
				}
				let value = match rule.and_then(|c| c.get(dir)) {
					Some(action) => {
						let v = self.field_value(action, from, *dty, &field_at)?;
						computed.push(quote!(let $(&local) = $v;));
						quote!($(&local))
					}
					None => {
						let nested = r.updated.get(&newer);
						let unbox = if self.names.boxed(from, &pds2_core::ast::Ident::new(&s)) {
							quote!(*$(self.names.field(from, &s)))
						} else {
							quote!($(self.names.field(from, &s)))
						};
						moved.push(self.names.field(from, &s));
						let (fs, ft) = (self.resolve(*sty), self.resolve(*dty));
						if rule.is_some() && fs != ft && self.numeric(fs) && self.numeric(ft) {
							self.cast(unbox, ft)
						} else {
							let conv = self.convert(step, dir, *sty, *dty, nested, false, &field_at);
							self.apply(&conv, unbox)
						}
					}
				};
				let value = if self.names.boxed(to, &pds2_core::ast::Ident::new(d)) {
					quote!(Box::new($value))
				} else {
					value
				};
				fields.push(quote!($(self.names.field(to, d)): $value));
			} else {
				let (kind, map) = match dir {
					Dir::Up => ("added", &r.added),
					Dir::Down => ("removed", &r.removed),
				};
				let Some(action) = map.get(d).and_then(|c| c.get(dir)) else {
					self.error(format!(
						"{field_at}: only in the {} version, add `{kind}({d}, {}: ...)`",
						if dir == Dir::Up { "newer" } else { "older" },
						dir.name()
					));
					continue;
				};
				self.used(rule_key(kind, d));
				let v = self.field_value(action, from, *dty, &field_at)?;
				computed.push(quote!(let $(&local) = $v;));
				let value = if self.names.boxed(to, &pds2_core::ast::Ident::new(d)) {
					quote!(Box::new($(&local)))
				} else {
					quote!($(&local))
				};
				fields.push(quote!($(self.names.field(to, d)): $value));
			}
		}
		for (s, _) in &src {
			if consumed.contains(s) {
				continue;
			}
			let (kind, map) = match dir {
				Dir::Up => ("removed", &r.removed),
				Dir::Down => ("added", &r.added),
			};
			if map.contains_key(s) {
				self.used(rule_key(kind, s));
			} else {
				self.error(format!(
					"{at}/{s}: only in the {} version, add `{kind}({s})`",
					if dir == Dir::Up { "older" } else { "newer" }
				));
			}
		}
		let (from_ty, to_ty) = (self.names.ty(from), self.names.ty(to));
		Some(quote! {
			{
				let _0 = &v;
				$(for c in computed => $c)
				let $from_ty { $(for m in &moved => $m,) .. } = v;
				$to_ty { $(for f in fields join(, ) => $f) }
			}
		})
	}

	fn union(
		&self,
		step: &Step,
		dir: Dir,
		from: TypeId,
		to: TypeId,
		rules: Option<&Rules>,
		root: bool,
		at: &str,
	) -> Option<rust::Tokens> {
		let uf = union::view(native(self.types, from)?)?;
		let ut = union::view(native(self.types, to)?)?;
		let empty = Rules::default();
		let r = rules.unwrap_or(&empty);
		let rule_key = |kind: &str, name: &str| format!("{}/{kind}({name})", r.path);
		let (from_ty, to_ty) = (self.names.ty(from), self.names.ty(to));
		let void = |id: TypeId| self.types.is_void(self.resolve(id));
		let target_name = |n: &str| -> String {
			match dir {
				Dir::Up => r.renamed.get(n).cloned().unwrap_or_else(|| n.to_owned()),
				Dir::Down => r
					.renamed
					.iter()
					.find(|(_, new)| *new == n)
					.map(|(old, _)| old.clone())
					.unwrap_or_else(|| n.to_owned()),
			}
		};
		let one = |t: rust::Tokens| if root { quote!(vec![$t]) } else { t };
		let mut arms = Vec::new();
		for (tag_value, name, ty) in &uf.arms {
			let n = name.as_str();
			let arm_at = format!("{at}/{n}");
			let src_pat = |bound: bool| {
				if void(ty.target) {
					quote!($(&from_ty)::$(self.names.variant(from, n)))
				} else if bound {
					quote!($(&from_ty)::$(self.names.variant(from, n))(p))
				} else {
					quote!($(&from_ty)::$(self.names.variant(from, n))(_))
				}
			};
			let t = target_name(n);
			let newer = match dir {
				Dir::Up => t.clone(),
				Dir::Down => n.to_owned(),
			};
			if t != n {
				let old = if dir == Dir::Up { n } else { t.as_str() };
				self.used(rule_key("renamed", old));
			}
			let override_ = r.converted.get(&newer).and_then(|c| c.get(dir));
			if r.converted.contains_key(&newer) {
				self.used(rule_key("converted", &newer));
			}
			let target = ut.arms.iter().find(|(_, m, _)| m.as_str() == t);
			let missing = match target {
				Some(_) => None,
				None => {
					let (kind, map) = match dir {
						Dir::Up => ("removed", &r.removed),
						Dir::Down => ("added", &r.added),
					};
					match map.get(n).and_then(|c| c.get(dir)) {
						Some(a) => {
							self.used(rule_key(kind, n));
							Some(a)
						}
						None => {
							self.error(format!(
								"{arm_at}: only in the {} version, add `{kind}({n}, {}: drop)` or custom(...)",
								if dir == Dir::Up { "older" } else { "newer" },
								dir.name()
							));
							continue;
						}
					}
				}
			};
			if let Some(action) = override_.or(missing) {
				let payload = if void(ty.target) { quote!(()) } else { quote!(p) };
				let unboxed = if !void(ty.target) && self.names.boxed(from, name) {
					quote!(*$payload)
				} else {
					payload
				};
				let body = match action {
					Action::Default => {
						let Some((tname, tty)) = ut.default.filter(|(_, t)| void(t.target)) else {
							self.error(format!("{arm_at}: `default` needs a payload-less default arm in the target"));
							continue;
						};
						let mir::Value::Signed(tag) = tag_value else {
							self.error(format!("{arm_at}: `default` needs a numeric tag"));
							continue;
						};
						let _ = tty;
						let tag = self.cast(quote!($(*tag as i64)), self.resolve(ut.tag_type));
						one(quote!($(&to_ty)::$(self.names.variant(to, tname.as_str()))($tag)))
					}
					Action::Drop if root => quote!(return Ok(vec![])),
					Action::Drop => quote!(return Err(crate::pds::convert::Error::Dropped)),
					Action::Unsupported => self.unsupported(&arm_at),
					Action::Custom(f) => {
						let f = self.custom(f);
						if root {
							quote!(return $f($unboxed, cx))
						} else {
							quote!($f($unboxed, cx)?)
						}
					}
					Action::Expr(_) => {
						let bind = match (void(ty.target), self.names.boxed(from, name)) {
							(true, _) => quote!(let _0 = &();),
							(false, true) => quote!(let _0 = &*p;),
							(false, false) => quote!(let _0 = &p;),
						};
						let value = match target {
							Some((_, tname, tty)) if !void(tty.target) => {
								let Some(v) = self.field_value(action, ty.target, tty.target, &arm_at) else { continue };
								let v = if self.names.boxed(to, tname) { quote!(Box::new($v)) } else { v };
								quote!($(&to_ty)::$(self.names.variant(to, tname.as_str()))($v))
							}
							Some(_) => {
								self.error(format!("{arm_at}: the target arm carries no data, rename it instead"));
								continue;
							}
							None => {
								let Some(v) = self.field_value(action, ty.target, to, &arm_at) else { continue };
								v
							}
						};
						let value = one(value);
						quote!({ $bind $value })
					}
				};
				arms.push(quote!($(src_pat(!void(ty.target))) => $body,));
				continue;
			}
			let (_, tname, tty) = target.expect("checked above");
			let to_arm = quote!($(&to_ty)::$(self.names.variant(to, tname.as_str())));
			match (void(ty.target), void(tty.target)) {
				(true, true) => arms.push(quote!($(src_pat(false)) => $(one(to_arm)),)),
				(false, false) => {
					let unbox = if self.names.boxed(from, name) { quote!(*p) } else { quote!(p) };
					let (fs, ft) = (self.resolve(ty.target), self.resolve(tty.target));
					let value = if r.converted.contains_key(&newer) && fs != ft && self.numeric(fs) && self.numeric(ft) {
						self.cast(unbox, ft)
					} else {
						let conv = self.convert(step, dir, ty.target, tty.target, r.updated.get(&newer), false, &arm_at);
						self.apply(&conv, unbox)
					};
					let value = if self.names.boxed(to, tname) { quote!(Box::new($value)) } else { value };
					arms.push(quote!($(src_pat(true)) => $(one(quote!($to_arm($value)))),));
				}
				_ => {
					self.error(format!("{arm_at}: payload appeared or disappeared, add `converted({n}, ...)`"));
				}
			}
		}
		if let Some((dname, dty)) = uf.default {
			let Some((tname, tty)) = ut.default else {
				self.error(format!("{at}: the default arm disappeared"));
				return None;
			};
			let tf = self.resolve(uf.tag_type);
			let tt = self.resolve(ut.tag_type);
			let tag = if tf == tt {
				quote!(t)
			} else if self.numeric(tf) && self.numeric(tt) {
				self.cast(quote!(t), tt)
			} else {
				self.error(format!("{at}: the tag type changed in a way that needs a rule"));
				return None;
			};
			let src_ty = &from_ty;
			let to_d = quote!($(&to_ty)::$(self.names.variant(to, tname.as_str())));
			let from_d = self.names.variant(from, dname.as_str());
			match (void(dty.target), void(tty.target)) {
				(true, true) => arms.push(quote!($src_ty::$(&from_d)(t) => $(one(quote!($to_d($tag)))),)),
				(false, false) => {
					let unbox = if self.names.boxed(from, dname) { quote!(*p) } else { quote!(p) };
					let conv = self.convert(step, dir, dty.target, tty.target, r.updated.get("default"), false, &format!("{at}/default"));
					let value = self.apply(&conv, unbox);
					let value = if self.names.boxed(to, tname) { quote!(Box::new($value)) } else { value };
					arms.push(quote!($src_ty::$(&from_d)(t, p) => $(one(quote!($to_d($tag, $value)))),));
				}
				_ => {
					self.error(format!("{at}: the default arm changed shape"));
					return None;
				}
			}
		}
		Some(quote! {
			Ok(match v {
				$(for a in arms => $a)
			})
		})
	}
}

fn root_variant(name: &str) -> String {
	name.split("::")
		.map(|p| {
			let mut c = p.chars();
			c.next()
				.map(|f| f.to_uppercase().collect::<String>() + c.as_str())
				.unwrap_or_default()
		})
		.collect()
}

pub fn generate(
	types: &TypeMap,
	reg: &Registry,
	merged: &Block,
	versions: &[crate::merge::Version],
	migrations: &[Option<Migration>],
	every: Option<&Migration>,
	virtual_packets: bool,
) -> Result<String> {
	let protocols: Vec<(i32, usize)> = versions
		.iter()
		.enumerate()
		.flat_map(|(i, v)| v.protocols.iter().map(move |p| (*p, i)))
		.collect();
	let legacy: Vec<(i32, usize)> = versions
		.iter()
		.enumerate()
		.flat_map(|(i, v)| v.legacy.iter().map(move |p| (*p, i)))
		.collect();
	let versions: Vec<String> = versions.iter().map(|v| v.name.clone()).collect();
	let variant_names: Vec<String> = versions.iter().map(|v| format!("V{}", v.replace(['.', '-'], "_"))).collect();
	ensure!(
		migrations.len() + 1 == versions.len(),
		"{} versions need {} migrations",
		versions.len(),
		versions.len() - 1
	);
	let tags: Vec<String> = versions.iter().map(|v| crate::merge::tag(v)).collect();
	let mut base = BTreeMap::new();
	fn defs(block: &Block, ns: &mut Vec<String>, out: &mut Vec<String>) {
		for stmt in &block.statements {
			let Some(Value::Item(head)) = stmt.items.first() else { continue };
			let Ok(name) = first_name(head) else { continue };
			match head.name.item.as_str() {
				"namespace" => {
					ns.push(name);
					defs(&head.block, ns, out);
					ns.pop();
				}
				"def" => out.push(ns.iter().chain([&name]).cloned().collect::<Vec<_>>().join("::")),
				_ => {}
			}
		}
	}
	let mut names = Vec::new();
	defs(merged, &mut Vec::new(), &mut names);
	for name in names {
		let stripped = tags
			.iter()
			.find_map(|t| name.strip_suffix(&format!("_{t}")))
			.unwrap_or(&name)
			.to_owned();
		base.insert(name, stripped);
	}
	let g = Gen {
		types,
		names: RustNames::new(types, reg, &["convert"])?,
		elab: Elaborator { types, reg },
		alias_base: {
			let mut found: BTreeMap<TypeId, Option<String>> = BTreeMap::new();
			for (id, def) in &types.0 {
				let TypeKind::Alias(_) = def.kind else { continue };
				let Some(name) = base.get(&full_name(types, *id)) else { continue };
				let target = Elaborator { types, reg }.resolve(*id);
				if base.contains_key(&full_name(types, target)) || matches!(&types[target].kind, TypeKind::Native(n) if n.specializer.is_none()) {
					continue;
				}
				found
					.entry(target)
					.and_modify(|e| {
						if e.as_ref() != Some(name) {
							*e = None;
						}
					})
					.or_insert_with(|| Some(name.clone()));
			}
			found.into_iter().filter_map(|(k, v)| Some((k, v?))).collect()
		},
		base,
		fns: RefCell::default(),
		code: RefCell::default(),
		calls: RefCell::default(),
		current: RefCell::default(),
		errors: RefCell::default(),
		used: RefCell::default(),
		dirty: RefCell::default(),
		counter: Cell::new(0),
	};
	let roots_of = |tag: &str| -> BTreeMap<String, TypeId> {
		types
			.0
			.iter()
			.filter_map(|(id, d)| {
				let full = full_name(types, *id);
				let rest = full.strip_prefix(&format!("{tag}::"))?;
				matches!(d.kind, TypeKind::Alias(_)).then(|| (rest.to_owned(), g.resolve(*id)))
			})
			.collect()
	};
	let mut dispatch = Vec::new();
	let mut arms = Vec::new();
	let mut root_names = BTreeSet::new();
	for (i, rules) in migrations.iter().enumerate() {
		let Some(rules) = rules else { continue };
		let mut merged = rules.rules.clone();
		for (name, hooks) in every.iter().flat_map(|e| &e.rules) {
			match merged.get_mut(name) {
				Some(existing) => merge_rules(existing, hooks.clone())?,
				None => {
					merged.insert(name.clone(), hooks.clone());
				}
			}
		}
		let step = Step {
			index: i,
			name: format!("{} -> {}", versions[i], versions[i + 1]),
			label: format!("{}_{}", tags[i], tags[i + 1].trim_start_matches('v')),
			rules: &merged,
		};
		let (a, b) = (roots_of(&tags[i]), roots_of(&tags[i + 1]));
		for (root, ta) in &a {
			let Some(tb) = b.get(root) else { continue };
			root_names.insert(root.clone());
			let variant = root_variant(root.trim_end_matches("::packet"));
			for (dir, from, to, fi, ti) in [(Dir::Up, *ta, *tb, i, i + 1), (Dir::Down, *tb, *ta, i + 1, i)] {
				let (fm, tm) = (g.names.ty(from), g.names.ty(to));
				let (fd, ts) = (g.names.item(from, "deserialize"), g.names.item(to, "serialize"));
				let body = if from == to && !g.dirty(&step, dir, from) {
					quote!(Ok(vec![data.to_vec()]))
				} else {
					let Conv::Call(f) = g.convert(&step, dir, from, to, None, true, root) else {
						unreachable!("roots always get a function");
					};
					quote! {
						let v: $fm = $fd(&mut crate::pds::Input::new(data))
							.map_err(|e| crate::pds::convert::Error::Encoding(format!("{e:?}")))?;
						let out: Vec<$tm> = match $f(v, cx) {
							Err(crate::pds::convert::Error::Dropped) => return Ok(vec![]),
							out => out?,
						};
						out.iter()
							.map(|p| {
								let mut o = crate::pds::Output::new();
								$ts(p, &mut o).map_err(|e| crate::pds::convert::Error::Encoding(format!("{e:?}")))?;
								Ok(o.into_inner())
							})
							.collect()
					}
				};
				let arm = format!("step_{}_{fi}_{ti}", variant.to_lowercase());
				dispatch.push(quote!((Root::$(&variant), $fi, $ti) => $(&arm)(data, cx),));
				arms.push(quote! {
					fn $(&arm)(data: &[u8], cx: &mut crate::Session) -> crate::pds::convert::Result<Vec<Vec<u8>>> {
						$body
					}
				});
			}
		}
		let unused: Vec<String> = rules.all.difference(&g.used.borrow()).cloned().collect();
		for u in unused {
			g.error(format!("{} -> {}: rule {u} is not used", versions[i], versions[i + 1]));
		}
	}
	for u in every.iter().flat_map(|e| e.all.difference(&g.used.borrow()).cloned().collect::<Vec<_>>()) {
		g.error(format!("every step: rule {u} is not used"));
	}
	let errors = g.errors.borrow();
	if !errors.is_empty() {
		bail!("migrations are incomplete:\n{}", errors.iter().cloned().collect::<Vec<_>>().join("\n"));
	}
	let variants: Vec<String> = root_names.iter().map(|r| root_variant(r.trim_end_matches("::packet"))).collect();
	let mut inspect = Vec::new();
	let mut names = Vec::new();
	for (i, tag) in tags.iter().enumerate() {
		for (root, id) in roots_of(tag) {
			if !root_names.contains(&root) {
				continue;
			}
			let variant = root_variant(root.trim_end_matches("::packet"));
			if let Some(u) = native(types, id).and_then(union::view) {
				if matches!(u.tag, union::TagView::Inline(_)) {
					let arms: Vec<(i128, String)> = u
						.arms
						.iter()
						.filter_map(|(v, n, _)| match v {
							mir::Value::Signed(t) => Some((*t, n.as_str().to_owned())),
							_ => None,
						})
						.collect();
					let de = g.names.item(u.tag_type, "deserialize");
					names.push(quote! {
						(Root::$(&variant), $i) => {
							let tag = $de(&mut input).ok()? as i64;
							let table: &[(i64, &str)] = &[$(for (t, n) in &arms join(, ) => ($(*t as i64), $(quoted(n))))];
							table
								.iter()
								.find(|(t, _)| *t == tag)
								.map(|(_, n)| *n)
						}
					});
				}
			}
			let de = g.names.item(id, "deserialize");
			let name = match native(types, id).and_then(union::view) {
				Some(_) => quote!($(g.names.item(id, "name"))(&p).to_owned()),
				None => quote! {
					format!("{p:?}").split(|c| c == '(' || c == ' ').next().unwrap_or_default().to_owned()
				},
			};
			let arm = format!("inspect_{}_{i}", variant.to_lowercase());
			inspect.push(quote!((Root::$(&variant), $i) => $(&arm)(data),));
			arms.push(quote! {
				fn $(&arm)(data: &[u8]) -> std::result::Result<String, String> {
					let mut input = crate::pds::Input::new(data);
					let p = $de(&mut input).map_err(|e| format!("{e:?}"))?;
					let name = $name;
					match input.remaining() {
						0 => Ok(name),
						n => Err(format!("{n} bytes left after {name}")),
					}
				}
			});
		}
	}
	let aliased: BTreeSet<usize> = migrations
		.iter()
		.enumerate()
		.filter(|(_, m)| m.is_some())
		.flat_map(|(i, _)| [i, i + 1])
		.collect();
	let version_mods: Vec<rust::Tokens> = aliased
		.iter()
		.map(|&i| {
			let mut names = BTreeMap::new();
			for id in roots_of(&tags[i]).into_values() {
				g.alias_names(id, None, &mut names);
			}
			let scope = Scope::new(Case::Snake, names.keys().map(String::as_str), &[])
				.with_context(|| format!("aliases of {}", tags[i]))?;
			Ok(quote! {
				pub mod $(&tags[i]) {
					$(for (name, id) in &names join($['\r']) => pub use super::$(g.names.module(*id)) as $(scope.get(name));)
				}
			})
		})
		.collect::<Result<_>>()?;
	let guarded = feedback_vertices(&g.calls.borrow());
	let code: Vec<rust::Tokens> = g.code.borrow().iter().map(|f| f.emit(guarded.contains(&f.name))).collect();
	let file: rust::Tokens = quote! {
		pub const VERSIONS: &[&str] = &[$(for v in &versions join(, ) => $(quoted(v)))];

		#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
		#[repr(usize)]
		pub enum Version {
			$(for t in &variant_names join(, ) => $t)
		}

		impl Version {
			pub const ALL: &[Version] = &[$(for t in &variant_names join(, ) => Version::$t)];

			pub fn name(self) -> &'static str {
				VERSIONS[self as usize]
			}

			pub fn index(self) -> usize {
				self as usize
			}

			pub fn at(index: usize) -> Option<Version> {
				Self::ALL.get(index).copied()
			}

			pub fn of(name: &str) -> Option<Version> {
				index_of(name).and_then(Self::at)
			}
		}

		pub const PROTOCOLS: &[(i32, usize)] = &[$(for (p, i) in &protocols join(, ) => ($(*p), $(*i)))];

		pub fn version_of(protocol: i32) -> Option<usize> {
			PROTOCOLS.iter().find(|(p, _)| *p == protocol).map(|(_, v)| *v)
		}

		pub const LEGACY_PROTOCOLS: &[(i32, usize)] = &[$(for (p, i) in &legacy join(, ) => ($(*p), $(*i)))];

		pub fn legacy_version_of(protocol: i32) -> Option<usize> {
			LEGACY_PROTOCOLS.iter().find(|(p, _)| *p == protocol).map(|(_, v)| *v)
		}

		pub fn index_of(version: &str) -> Option<usize> {
			VERSIONS.iter().position(|v| *v == version)
		}

		pub const STEPS: &[bool] = &[$(for m in migrations join(, ) => $(if m.is_some() { true } else { false }))];

		pub fn reachable(a: usize, b: usize) -> bool {
			a.max(b) < VERSIONS.len() && STEPS[a.min(b)..a.max(b)].iter().all(|s| *s)
		}

		#[derive(Debug, Clone, Copy, PartialEq, Eq)]
		pub enum Root {
			$(for v in &variants join(, ) => $v)
		}

		impl Root {
			pub fn to_client(self) -> bool {
				matches!(self, $(for v in variants.iter().filter(|v| v.ends_with("ToClient")) join( | ) => Root::$v))
			}
		}

		pub fn step(root: Root, from: usize, to: usize, data: &[u8], cx: &mut crate::Session) -> crate::pds::convert::Result<Vec<Vec<u8>>> {
			match (root, from, to) {
				$(for d in dispatch join($['\r']) => $d)
				_ => crate::pds::convert::unconvertible(format!("no step {from} -> {to} for {root:?}")),
			}
		}

		pub fn packet_name(root: Root, version: usize, data: &[u8]) -> Option<&'static str> {
			let mut input = crate::pds::Input::new(data);
			match (root, version) {
				$(for n in names => $n)
				_ => None,
			}
		}

		pub fn inspect(root: Root, version: usize, data: &[u8]) -> std::result::Result<String, String> {
			match (root, version) {
				$(for i in inspect join($['\r']) => $i)
				_ => Err(format!("{root:?} does not exist in {}", VERSIONS[version])),
			}
		}

		$(if virtual_packets {
			pub enum Carrier {
				Wire(Vec<u8>),
				Virtual(crate::custom::Virtual),
			}

			pub fn convert(root: Root, from: usize, to: usize, data: &[u8], cx: &mut crate::Session) -> crate::pds::convert::Result<Vec<Vec<u8>>> {
				let packets = match crate::custom::virtual_start(root, from, to, data, cx)? {
					Some(carried) => carried,
					None => vec![Carrier::Wire(data.to_vec())],
				};
				carry(root, from, to, packets, cx)
			}

			pub fn carry(root: Root, from: usize, to: usize, mut packets: Vec<Carrier>, cx: &mut crate::Session) -> crate::pds::convert::Result<Vec<Vec<u8>>> {
				let mut at = from;
				while at != to {
					let next = if to > at { at + 1 } else { at - 1 };
					let mut out = Vec::new();
					for p in packets {
						match p {
							Carrier::Wire(bytes) => match crate::custom::virtual_wire(root, at, next, &bytes, cx)? {
								Some(carried) => out.extend(carried),
								None => out.extend(step(root, at, next, &bytes, cx)?.into_iter().map(Carrier::Wire)),
							},
							Carrier::Virtual(model) => out.extend(crate::custom::virtual_step(root, at, next, model, cx)?),
						}
					}
					packets = out;
					at = next;
				}
				let mut wire = Vec::new();
				for p in packets {
					match p {
						Carrier::Wire(bytes) => wire.push(bytes),
						Carrier::Virtual(model) => wire.extend(crate::custom::virtual_finish(root, to, model, cx)?),
					}
				}
				Ok(wire)
			}
		} else {
			pub fn convert(root: Root, from: usize, to: usize, data: &[u8], cx: &mut crate::Session) -> crate::pds::convert::Result<Vec<Vec<u8>>> {
				let mut packets = vec![data.to_vec()];
				let mut at = from;
				while at != to {
					let next = if to > at { at + 1 } else { at - 1 };
					let mut out = Vec::new();
					for p in &packets {
						out.extend(step(root, at, next, p, cx)?);
					}
					packets = out;
					at = next;
				}
				Ok(packets)
			}
		})

		$(for m in version_mods join($['\n']) => $m)

		$(for a in arms join($['\n']) => $a)

		$(for c in code.iter() join($['\n']) => $c)
	};
	file.to_file_string().context("rendering converters")
}
