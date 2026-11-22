use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use genco::lang::Rust;

use crate::ast::Ident;
use crate::hir::TypeId;
use crate::mir::{Expr, TypeKind, TypeMap};
use crate::specializer::{Embed, Registry, Scalar, SlotVisitor, ViewPolicy};

use super::boxing::{Boxed, visit_type};
use super::common::scalar_of;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	Same,
	Own,
	Native,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Into {
	Module,
	Take,
	Copy,
	Alias(TypeId),
}

pub struct ViewSet {
	pub included: BTreeSet<TypeId>,
	pub kinds: BTreeMap<TypeId, Kind>,
	pub into: BTreeMap<TypeId, Into>,
	pub moves: BTreeSet<TypeId>,
	pub roots: Vec<(String, TypeId)>,
}

impl ViewSet {
	pub fn moves(&self, id: TypeId) -> bool {
		self.moves.contains(&id)
	}
	pub fn kind(&self, id: TypeId) -> Kind {
		self.kinds.get(&id).copied().unwrap_or(Kind::Same)
	}
	pub fn into(&self, id: TypeId) -> Into {
		match self.into.get(&id).copied().unwrap_or(Into::Copy) {
			Into::Alias(t) => self.into(t),
			i => i,
		}
	}
	#[allow(clippy::wrong_self_convention)]
	pub fn into_target(&self, id: TypeId) -> TypeId {
		match self.into.get(&id) {
			Some(Into::Alias(t)) => self.into_target(*t),
			_ => id,
		}
	}
	pub fn emitted(&self, id: TypeId) -> bool {
		!self.same(id) || self.into.get(&id) == Some(&Into::Module)
	}
	pub fn same(&self, id: TypeId) -> bool {
		self.kind(id) == Kind::Same
	}
	pub fn borrows(&self, id: TypeId) -> bool {
		!self.same(id)
	}
}

struct Slots<'s>(&'s mut BTreeSet<TypeId>);
impl SlotVisitor for Slots<'_> {
	fn slot(&mut self, _name: &Ident, target: TypeId, _embed: Embed) {
		self.0.insert(target);
	}
}

fn expr_types(e: &Expr, out: &mut BTreeSet<TypeId>) {
	e.walk(&mut |x| {
		out.extend(x.ty());
		if let Expr::Field(f) = x {
			for item in &f.0 {
				out.extend([item.from.get(), item.to.get()]);
			}
		}
	});
}

pub(super) fn children(types: &TypeMap, reg: &Registry, id: TypeId) -> Result<BTreeSet<TypeId>> {
	let mut out = BTreeSet::new();
	match &types[id].kind {
		TypeKind::Alias(t) => {
			out.insert(t.target);
		}
		TypeKind::Native(n) => {
			out.extend(
				n.specialization
					.iter()
					.chain(&n.fields)
					.map(|(_, t)| t.target),
			);
			visit_type(types, reg, id, &mut Slots(&mut out))?;
		}
	}
	out.retain(|t| t.is_valid());
	Ok(out)
}

pub(super) fn references(types: &TypeMap, reg: &Registry, id: TypeId) -> Result<BTreeSet<TypeId>> {
	let def = &types[id];
	let mut out = children(types, reg, id)?;
	out.extend(def.params.iter().chain(&def.param_types).map(|(_, t)| *t));
	if let TypeKind::Native(n) = &def.kind {
		out.extend(n.virtual_fields.iter().map(|(_, t)| t.target));
		out.extend(n.arg_types.iter().map(|(_, t)| *t));
	}
	for e in def.exprs() {
		expr_types(e, &mut out);
	}
	out.retain(|t| t.is_valid() && types.0.contains_key(t));
	Ok(out)
}

fn native_attr(types: &TypeMap, id: TypeId, value: &str) -> bool {
	let TypeKind::Native(n) = &types[id].kind else {
		return false;
	};
	n.specializer.is_none()
		&& n.attr("view")
			.is_some_and(|v| v.iter().any(|i| i.as_str() == value))
}

fn native_moves(types: &TypeMap, id: TypeId) -> bool {
	native_attr(types, id, "take")
}

fn native_view(types: &TypeMap, id: TypeId) -> bool {
	let raw = matches!(&types[id].kind, TypeKind::Native(n) if n.specializer.is_none())
		&& matches!(scalar_of(types, id), Some(Scalar::Bytes | Scalar::String));
	raw || native_attr(types, id, "borrow") || native_moves(types, id)
}

pub fn analyze(types: &TypeMap, reg: &Registry, boxed: &Boxed, namespace: &str) -> Result<ViewSet> {
	let prefix: Vec<&str> = namespace.split("::").filter(|s| !s.is_empty()).collect();
	let mut roots = Vec::new();
	let mut names = BTreeMap::new();
	for (id, def) in &types.0 {
		let path: Vec<&str> = def.name.path.iter().map(|i| i.as_str()).collect();
		if !path.starts_with(&prefix) {
			continue;
		}
		roots.push(*id);
		if matches!(def.kind, TypeKind::Alias(_)) {
			let name = path[prefix.len()..]
				.iter()
				.copied()
				.chain([def.name.item.as_str()])
				.collect::<Vec<_>>()
				.join("_");
			names.entry(name).or_insert_with(Vec::new).push(*id);
		}
	}
	if roots.is_empty() {
		bail!("no types under namespace {namespace:?}");
	}

	let mut included = BTreeSet::new();
	let mut queue = roots;
	while let Some(id) = queue.pop() {
		if included.insert(id) {
			queue.extend(references(types, reg, id)?);
		}
	}

	let mut kids = BTreeMap::new();
	for id in &included {
		kids.insert(*id, children(types, reg, *id)?);
	}
	let policy = |id: TypeId| match &types[id].kind {
		TypeKind::Alias(_) => (ViewPolicy::Composite, false),
		TypeKind::Native(n) => n
			.specializer
			.as_ref()
			.and_then(|(s, _)| reg.codegen::<Rust>(s.as_str()))
			.map_or((ViewPolicy::Opaque, false), |c| {
				(c.view_policy(), c.view_borrows(types, n))
			}),
	};
	let mut borrowing: BTreeSet<TypeId> = included
		.iter()
		.copied()
		.filter(|id| native_view(types, *id) || policy(*id).1)
		.collect();
	loop {
		let before = borrowing.len();
		for id in &included {
			if !borrowing.contains(id)
				&& policy(*id).0 == ViewPolicy::Composite
				&& kids[id].iter().any(|k| borrowing.contains(k))
			{
				borrowing.insert(*id);
			}
		}
		if borrowing.len() == before {
			break;
		}
	}
	let kinds: BTreeMap<TypeId, Kind> = included
		.iter()
		.map(|id| {
			let kind = if native_view(types, *id) {
				Kind::Native
			} else if borrowing.contains(id) {
				Kind::Own
			} else {
				Kind::Same
			};
			(*id, kind)
		})
		.collect();

	let codegen = |id: TypeId| match &types[id].kind {
		TypeKind::Native(n) => n
			.specializer
			.as_ref()
			.and_then(|(s, _)| reg.codegen::<Rust>(s.as_str())),
		TypeKind::Alias(_) => None,
	};
	let boxed_owners: BTreeSet<TypeId> = boxed.iter().map(|(owner, _)| *owner).collect();
	let mut heap: BTreeSet<TypeId> = included
		.iter()
		.copied()
		.filter(|id| match &types[*id].kind {
			TypeKind::Alias(_) => false,
			TypeKind::Native(n) => match codegen(*id) {
				Some(c) => c.view_heap() || boxed_owners.contains(id),
				None => n.specializer.is_some() || !copied(types, *id),
			},
		})
		.collect();
	loop {
		let before = heap.len();
		for id in &included {
			if !heap.contains(id) && kids[id].iter().any(|k| heap.contains(k)) {
				heap.insert(*id);
			}
		}
		if heap.len() == before {
			break;
		}
	}
	let into: BTreeMap<TypeId, Into> = included
		.iter()
		.map(|id| {
			let action = match kinds[id] {
				Kind::Own | Kind::Native => Into::Module,
				Kind::Same if !heap.contains(id) => Into::Copy,
				Kind::Same => match (&types[*id].kind, codegen(*id)) {
					(TypeKind::Alias(t), _) => Into::Alias(t.target),
					(_, Some(c)) if c.view_take() => Into::Take,
					(_, Some(c)) if c.view_policy() == ViewPolicy::Composite => Into::Module,
					_ => Into::Copy,
				},
			};
			(*id, action)
		})
		.collect();
	let mut moves: BTreeSet<TypeId> = included
		.iter()
		.copied()
		.filter(|id| match kinds[id] {
			Kind::Native => native_moves(types, *id),
			Kind::Same => matches!(into[id], Into::Take | Into::Module),
			Kind::Own => codegen(*id).is_some_and(|c| c.view_moves()),
		})
		.collect();
	loop {
		let before = moves.len();
		for id in &included {
			let propagates = kinds[id] == Kind::Own || matches!(into[id], Into::Alias(_));
			if propagates && !moves.contains(id) && kids[id].iter().any(|k| moves.contains(k)) {
				moves.insert(*id);
			}
		}
		if moves.len() == before {
			break;
		}
	}

	let roots = names
		.into_iter()
		.filter_map(|(name, ids)| match ids.as_slice() {
			[id] if !name.starts_with("pds_") => Some((name, *id)),
			_ => None,
		})
		.collect();
	Ok(ViewSet {
		included,
		kinds,
		into,
		moves,
		roots,
	})
}

fn copied(types: &TypeMap, id: TypeId) -> bool {
	types.is_void(id) || scalar_of(types, id).is_some_and(|s| s.is_number() || s == Scalar::Bool)
}
