use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use genco::lang::rust;
use genco::prelude::*;
use pds2_core::ast::{Block, FieldRefSegment, Item, Value};
use pds2_core::generator::common::scalar_of;
use pds2_core::generator::rust::{rname, RustNames};
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
	pub whole: Change,
	pub added: BTreeMap<String, Change>,
	pub removed: BTreeMap<String, Change>,
	pub converted: BTreeMap<String, Change>,
	pub renamed: BTreeMap<String, String>,
	pub updated: BTreeMap<String, Rules>,
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
			"added" => {
				r.added.insert(name, change(op));
			}
			"removed" => {
				r.removed.insert(name, change(op));
			}
			"converted" => {
				r.converted.insert(name, change(op));
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
				r.updated.insert(name.clone(), parse_rules(op, format!("{path}/{name}"), all)?);
			}
			k => bail!("{path}: unknown rule `{k}`"),
		}
	}
	Ok(r)
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
					ensure!(out.insert(full.clone(), rules).is_none(), "{full} is updated twice");
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

#[derive(Clone)]
enum Conv {
	Identity,
	Call(String),
}

struct Gen<'a> {
	types: &'a TypeMap,
	names: RustNames<'a>,
	elab: Elaborator<'a>,
	base: BTreeMap<String, String>,
	fns: RefCell<BTreeMap<(usize, Dir, TypeId, TypeId, bool), String>>,
	code: RefCell<Vec<rust::Tokens>>,
	errors: RefCell<BTreeSet<String>>,
	used: RefCell<BTreeSet<String>>,
	dirty: RefCell<BTreeMap<(usize, TypeId), bool>>,
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

	fn dirty(&self, step: &Step, id: TypeId) -> bool {
		let id = self.resolve(id);
		if let Some(d) = self.dirty.borrow().get(&(step.index, id)) {
			return *d;
		}
		self.dirty.borrow_mut().insert((step.index, id), false);
		let own = self
			.base
			.get(&full_name(self.types, id))
			.is_some_and(|b| step.rules.contains_key(b));
		let d = own || self.children(id).into_iter().any(|c| self.dirty(step, c));
		self.dirty.borrow_mut().insert((step.index, id), d);
		d
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
		format!("{}_{}_{}_{n}", dir.name(), step.label, self.names.module(to).trim_start_matches("pds_"))
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
		let base_of = |id: TypeId| {
			self.base
				.get(&full_name(self.types, id))
				.or_else(|| self.base.get(&full_name(self.types, self.resolve(id))))
		};
		let (bf, bt) = (base_of(from), base_of(to));
		let (from, to) = (self.resolve(from), self.resolve(to));
		if let Some(r) = rules {
			self.used(r.path.clone());
		}
		if from == to && !root && rules.is_none() && !self.dirty(step, from) {
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
			None if same => bt.and_then(|b| step.rules.get(b)).inspect(|r| self.used(r.path.clone())),
			None => None,
		};
		if from == to && !root && rules.is_none() && !self.dirty(step, from) {
			return Conv::Identity;
		}
		let key = (step.index, dir, from, to, root);
		if let Some(name) = self.fns.borrow().get(&key) {
			return Conv::Call(name.clone());
		}
		let name = self.fresh(step, dir, to);
		self.fns.borrow_mut().insert(key, name.clone());
		let (from_ty, to_ty) = (self.names.ty(from), self.names.ty(to));
		let ret = if root { quote!(Vec<$(&to_ty)>) } else { to_ty.clone() };
		let body = self.body(step, dir, from, to, rules, root, at);
		let body = body.unwrap_or_else(|| quote!(unreachable!()));
		self.code.borrow_mut().push(quote! {
			pub fn $(&name)(v: $from_ty, cx: &mut crate::Session) -> crate::pds::convert::Result<$ret> {
				$body
			}
		});
		Conv::Call(name)
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
		if from == to && rules.is_none() && !self.dirty(step, from) {
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
				match self.convert(step, dir, ef, et, rules.and_then(|r| r.updated.get("items")), false, &format!("{at}/items")) {
					Conv::Identity => Some(wrap(quote!(v))),
					Conv::Call(f) => Some(if root {
						quote!(Ok(vec![v.into_iter().map(|x| $f(x, cx)).collect::<crate::pds::convert::Result<Vec<_>>>()?]))
					} else {
						quote!(v.into_iter().map(|x| $f(x, cx)).collect())
					}),
				}
			}
			(Some("option"), Some("option")) => {
				let (ef, et) = (self.element(from)?, self.element(to)?);
				let slot = pds2_core::ast::Ident::new(VALUE);
				let unbox = if self.names.boxed(from, &slot) { quote!(*x) } else { quote!(x) };
				let rebox = |t: rust::Tokens| if self.names.boxed(to, &slot) { quote!(Box::new($t)) } else { t };
				let conv = self.convert(step, dir, ef, et, rules.and_then(|r| r.updated.get("value")), false, &format!("{at}/value"));
				let inner = rebox(self.apply(&conv, unbox));
				let mapped = quote!(v.map(|x| -> crate::pds::convert::Result<_> { Ok($inner) }).transpose()?);
				Some(wrap(mapped))
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
							quote!(*$(rname(&pds2_core::ast::Ident::new(&s))))
						} else {
							quote!($(rname(&pds2_core::ast::Ident::new(&s))))
						};
						moved.push(rname(&pds2_core::ast::Ident::new(&s)));
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
				fields.push(quote!($(rname(&pds2_core::ast::Ident::new(d))): $value));
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
				fields.push(quote!($(rname(&pds2_core::ast::Ident::new(d))): $value));
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
					quote!($(&from_ty)::$(rname(name)))
				} else if bound {
					quote!($(&from_ty)::$(rname(name))(p))
				} else {
					quote!($(&from_ty)::$(rname(name))(_))
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
						one(quote!($(&to_ty)::$(rname(tname))($tag)))
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
					_ => {
						self.error(format!("{arm_at}: use drop (packets only) or custom(...)"));
						continue;
					}
				};
				arms.push(quote!($(src_pat(!void(ty.target))) => $body,));
				continue;
			}
			let (_, tname, tty) = target.expect("checked above");
			let to_arm = quote!($(&to_ty)::$(rname(tname)));
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
			let to_d = quote!($(&to_ty)::$(rname(tname)));
			match (void(dty.target), void(tty.target)) {
				(true, true) => arms.push(quote!($src_ty::$(rname(dname))(t) => $(one(quote!($to_d($tag)))),)),
				(false, false) => {
					let unbox = if self.names.boxed(from, dname) { quote!(*p) } else { quote!(p) };
					let conv = self.convert(step, dir, dty.target, tty.target, r.updated.get("default"), false, &format!("{at}/default"));
					let value = self.apply(&conv, unbox);
					let value = if self.names.boxed(to, tname) { quote!(Box::new($value)) } else { value };
					arms.push(quote!($src_ty::$(rname(dname))(t, p) => $(one(quote!($to_d($tag, $value)))),));
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
	versions: &[(String, Vec<i32>)],
	migrations: &[Option<Migration>],
) -> Result<String> {
	let protocols: Vec<(i32, usize)> = versions
		.iter()
		.enumerate()
		.flat_map(|(i, (_, ps))| ps.iter().map(move |p| (*p, i)))
		.collect();
	let versions: Vec<String> = versions.iter().map(|(v, _)| v.clone()).collect();
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
		names: RustNames::new(types, reg)?,
		elab: Elaborator { types, reg },
		base,
		fns: RefCell::default(),
		code: RefCell::default(),
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
	let mut root_names = BTreeSet::new();
	for (i, rules) in migrations.iter().enumerate() {
		let Some(rules) = rules else { continue };
		let step = Step {
			index: i,
			name: format!("{} -> {}", versions[i], versions[i + 1]),
			label: format!("{}_{}", tags[i], tags[i + 1].trim_start_matches('v')),
			rules: &rules.rules,
		};
		let (a, b) = (roots_of(&tags[i]), roots_of(&tags[i + 1]));
		for (root, ta) in &a {
			let Some(tb) = b.get(root) else { continue };
			root_names.insert(root.clone());
			let variant = root_variant(root.trim_end_matches("::packet"));
			for (dir, from, to, fi, ti) in [(Dir::Up, *ta, *tb, i, i + 1), (Dir::Down, *tb, *ta, i + 1, i)] {
				let (fm, tm) = (g.names.ty(from), g.names.ty(to));
				let (fd, ts) = (g.names.item(from, "deserialize"), g.names.item(to, "serialize"));
				let body = if from == to && !g.dirty(&step, from) {
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
				dispatch.push(quote!((Root::$(&variant), $fi, $ti) => { $body }));
			}
		}
		let unused: Vec<String> = rules.all.difference(&g.used.borrow()).cloned().collect();
		for u in unused {
			g.error(format!("{} -> {}: rule {u} is not used", versions[i], versions[i + 1]));
		}
	}
	let errors = g.errors.borrow();
	if !errors.is_empty() {
		bail!("migrations are incomplete:\n{}", errors.iter().cloned().collect::<Vec<_>>().join("\n"));
	}
	let variants: Vec<String> = root_names.iter().map(|r| root_variant(r.trim_end_matches("::packet"))).collect();
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
			quote! {
				pub mod $(&tags[i]) {
					$(for (name, id) in &names join($['\r']) => pub use super::super::$(g.names.module(*id)) as $(rname(&pds2_core::ast::Ident::new(name)));)
				}
			}
		})
		.collect();
	let code = g.code.borrow();
	let file: rust::Tokens = quote! {
		pub const VERSIONS: &[&str] = &[$(for v in &versions join(, ) => $(quoted(v)))];

		pub const PROTOCOLS: &[(i32, usize)] = &[$(for (p, i) in &protocols join(, ) => ($(*p), $(*i)))];

		pub fn version_of(protocol: i32) -> Option<usize> {
			PROTOCOLS.iter().find(|(p, _)| *p == protocol).map(|(_, v)| *v)
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

		pub fn step(root: Root, from: usize, to: usize, data: &[u8], cx: &mut crate::Session) -> crate::pds::convert::Result<Vec<Vec<u8>>> {
			match (root, from, to) {
				$(for d in dispatch => $d)
				_ => crate::pds::convert::unconvertible(format!("no step {from} -> {to} for {root:?}")),
			}
		}

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

		$(for m in version_mods join($['\n']) => $m)

		$(for c in code.iter() join($['\n']) => $c)
	};
	file.to_file_string().context("rendering converters")
}
