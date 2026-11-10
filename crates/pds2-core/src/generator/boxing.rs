use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};

use crate::ast::Ident;
use crate::hir::TypeId;
use crate::mir::{TypeKind, TypeMap};
use crate::specializer::{BoxCost, Embed, Registry, SlotVisitor};

pub type Boxed = BTreeSet<(TypeId, Ident)>;

pub fn visit_type(
	types: &TypeMap,
	reg: &Registry,
	id: TypeId,
	v: &mut dyn SlotVisitor,
) -> Result<()> {
	match &types[id].kind {
		TypeKind::Alias(t) => v.slot(&Ident::new("@alias"), t.target, Embed::Fixed),
		TypeKind::Native(native) => {
			if let Some((specializer, _)) = &native.specializer {
				let Some(layout) = reg.layout(specializer.as_str()) else {
					bail!("no layout for specializer {specializer:?}");
				};
				layout.visit(native, v);
			}
		}
	}
	Ok(())
}

struct Edge {
	owner: TypeId,
	slot: Ident,
	target: TypeId,
	embed: Embed,
	boxed: bool,
}
impl Edge {
	fn by_value(&self) -> bool {
		self.embed != Embed::Indirect && !self.boxed
	}
}

struct Collect<'e> {
	owner: TypeId,
	edges: &'e mut Vec<Edge>,
}
impl SlotVisitor for Collect<'_> {
	fn slot(&mut self, name: &Ident, target: TypeId, embed: Embed) {
		self.edges.push(Edge {
			owner: self.owner,
			slot: name.clone(),
			target,
			embed,
			boxed: false,
		});
	}
}

pub fn analyze(types: &TypeMap, reg: &Registry) -> Result<Boxed> {
	let mut edges = Vec::new();
	for id in types.0.keys() {
		visit_type(
			types,
			reg,
			*id,
			&mut Collect {
				owner: *id,
				edges: &mut edges,
			},
		)?;
	}

	for cost in [BoxCost::Low, BoxCost::High] {
		loop {
			let comp = components(&edges);
			let Some(e) = edges
				.iter_mut()
				.find(|e| e.embed == Embed::Boxable(cost) && !e.boxed && same_component(&comp, e))
			else {
				break;
			};
			e.boxed = true;
		}
	}

	let comp = components(&edges);
	if let Some(e) = edges
		.iter()
		.find(|e| e.by_value() && same_component(&comp, e))
	{
		let cycle: Vec<String> = comp
			.iter()
			.filter(|(_, c)| **c == comp[&e.owner])
			.map(|(id, _)| types[*id].name.readable_name())
			.collect();
		bail!(
			"infinitely sized type: {:?} holds itself by value through {:?}, cycle {cycle:?} has no boxable slot",
			types[e.owner].name.readable_name(),
			e.slot.as_str(),
		);
	}

	Ok(edges
		.into_iter()
		.filter(|e| e.boxed)
		.map(|e| (e.owner, e.slot))
		.collect())
}

fn same_component(comp: &BTreeMap<TypeId, usize>, e: &Edge) -> bool {
	matches!((comp.get(&e.owner), comp.get(&e.target)), (Some(a), Some(b)) if a == b)
}

fn components(edges: &[Edge]) -> BTreeMap<TypeId, usize> {
	let mut adj: BTreeMap<TypeId, Vec<TypeId>> = BTreeMap::new();
	for e in edges.iter().filter(|e| e.by_value()) {
		adj.entry(e.owner).or_default().push(e.target);
		adj.entry(e.target).or_default();
	}
	let mut t = Tarjan {
		adj: &adj,
		index: BTreeMap::new(),
		low: BTreeMap::new(),
		stack: Vec::new(),
		on_stack: BTreeSet::new(),
		comp: BTreeMap::new(),
	};
	for v in adj.keys() {
		if !t.index.contains_key(v) {
			t.strong(*v);
		}
	}
	t.comp
}

struct Tarjan<'a> {
	adj: &'a BTreeMap<TypeId, Vec<TypeId>>,
	index: BTreeMap<TypeId, usize>,
	low: BTreeMap<TypeId, usize>,
	stack: Vec<TypeId>,
	on_stack: BTreeSet<TypeId>,
	comp: BTreeMap<TypeId, usize>,
}
impl Tarjan<'_> {
	fn strong(&mut self, v: TypeId) {
		let i = self.index.len();
		self.index.insert(v, i);
		self.low.insert(v, i);
		self.stack.push(v);
		self.on_stack.insert(v);
		for &w in &self.adj[&v] {
			if !self.index.contains_key(&w) {
				self.strong(w);
				let lw = self.low[&w];
				let lv = self.low.get_mut(&v).expect("visited");
				*lv = (*lv).min(lw);
			} else if self.on_stack.contains(&w) {
				let iw = self.index[&w];
				let lv = self.low.get_mut(&v).expect("visited");
				*lv = (*lv).min(iw);
			}
		}
		if self.low[&v] == self.index[&v] {
			let c = i;
			loop {
				let w = self.stack.pop().expect("v is on the stack");
				self.on_stack.remove(&w);
				self.comp.insert(w, c);
				if w == v {
					break;
				}
			}
		}
	}
}
