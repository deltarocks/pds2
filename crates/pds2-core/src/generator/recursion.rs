use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::hir::TypeId;
use crate::mir::{TypeKind, TypeMap};
use crate::specializer::{Embed, Registry, SlotVisitor};

use super::boxing::visit_type;

pub type Graph<N> = BTreeMap<N, BTreeSet<N>>;

pub fn components<N: Ord + Clone>(adj: &Graph<N>) -> BTreeMap<N, usize> {
	let mut t = Tarjan {
		adj,
		index: BTreeMap::new(),
		low: BTreeMap::new(),
		stack: Vec::new(),
		on_stack: BTreeSet::new(),
		comp: BTreeMap::new(),
	};
	for v in adj.keys() {
		if !t.index.contains_key(v) {
			t.strong(v.clone());
		}
	}
	t.comp
}

pub fn feedback_vertices<N: Ord + Clone>(adj: &Graph<N>) -> BTreeSet<N> {
	let mut adj = adj.clone();
	let mut picked = BTreeSet::new();
	loop {
		let comp = components(&adj);
		let mut degree: BTreeMap<&N, (usize, usize)> = BTreeMap::new();
		for (u, outs) in &adj {
			for v in outs.iter().filter(|v| comp[*v] == comp[u]) {
				degree.entry(u).or_default().1 += 1;
				degree.entry(v).or_default().0 += 1;
			}
		}
		let best = degree
			.into_iter()
			.max_by(|(a, (ai, ao)), (b, (bi, bo))| (ai * ao).cmp(&(bi * bo)).then_with(|| b.cmp(a)))
			.map(|(n, _)| n.clone());
		let Some(best) = best else {
			return picked;
		};
		adj.remove(&best);
		for outs in adj.values_mut() {
			outs.remove(&best);
		}
		picked.insert(best);
	}
}

struct Collect<'g> {
	owner: TypeId,
	adj: &'g mut Graph<TypeId>,
}
impl SlotVisitor for Collect<'_> {
	fn slot(&mut self, _name: &crate::ast::Ident, target: TypeId, _embed: Embed) {
		self.adj.entry(self.owner).or_default().insert(target);
	}
}

pub fn guarded(types: &TypeMap, reg: &Registry) -> Result<BTreeSet<TypeId>> {
	let mut adj: Graph<TypeId> = BTreeMap::new();
	for (id, def) in &types.0 {
		adj.entry(*id).or_default();
		match &def.kind {
			TypeKind::Alias(t) => {
				adj.entry(*id).or_default().insert(t.target);
			}
			TypeKind::Native(n) if n.specializer.is_some() => {
				let uses = n
					.specialization
					.iter()
					.chain(&n.fields)
					.map(|(_, t)| t.target);
				adj.entry(*id).or_default().extend(uses);
				visit_type(
					types,
					reg,
					*id,
					&mut Collect {
						owner: *id,
						adj: &mut adj,
					},
				)?;
			}
			TypeKind::Native(_) => {}
		}
	}
	Ok(feedback_vertices(&adj))
}

struct Tarjan<'a, N> {
	adj: &'a Graph<N>,
	index: BTreeMap<N, usize>,
	low: BTreeMap<N, usize>,
	stack: Vec<N>,
	on_stack: BTreeSet<N>,
	comp: BTreeMap<N, usize>,
}
impl<N: Ord + Clone> Tarjan<'_, N> {
	fn strong(&mut self, v: N) {
		let i = self.index.len();
		self.index.insert(v.clone(), i);
		self.low.insert(v.clone(), i);
		self.stack.push(v.clone());
		self.on_stack.insert(v.clone());
		for w in self.adj.get(&v).into_iter().flatten() {
			if !self.index.contains_key(w) {
				self.strong(w.clone());
				let lw = self.low[w];
				let lv = self.low.get_mut(&v).expect("visited");
				*lv = (*lv).min(lw);
			} else if self.on_stack.contains(w) {
				let iw = self.index[w];
				let lv = self.low.get_mut(&v).expect("visited");
				*lv = (*lv).min(iw);
			}
		}
		if self.low[&v] == self.index[&v] {
			loop {
				let w = self.stack.pop().expect("v is on the stack");
				self.on_stack.remove(&w);
				let done = w == v;
				self.comp.insert(w, i);
				if done {
					break;
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn graph(edges: &[(u32, u32)]) -> Graph<u32> {
		let mut g = Graph::new();
		for (a, b) in edges {
			g.entry(*a).or_default().insert(*b);
			g.entry(*b).or_default();
		}
		g
	}

	fn acyclic_without(g: &Graph<u32>, cut: &BTreeSet<u32>) -> bool {
		let mut g = g.clone();
		g.retain(|n, _| !cut.contains(n));
		for outs in g.values_mut() {
			outs.retain(|n| !cut.contains(n));
		}
		let comp = components(&g);
		g.iter()
			.all(|(u, outs)| outs.iter().all(|v| comp[u] != comp[v]))
	}

	#[test]
	fn acyclic_needs_no_guards() {
		assert!(feedback_vertices(&graph(&[(1, 2), (2, 3), (1, 3)])).is_empty());
	}

	#[test]
	fn self_loop_is_guarded() {
		assert_eq!(
			feedback_vertices(&graph(&[(1, 1), (1, 2)])),
			BTreeSet::from([1])
		);
	}

	#[test]
	fn shared_hub_breaks_every_cycle() {
		let g = graph(&[(1, 2), (2, 1), (2, 3), (3, 2), (2, 4), (4, 2)]);
		assert_eq!(feedback_vertices(&g), BTreeSet::from([2]));
	}

	#[test]
	fn nbt_like_cycles_are_all_broken() {
		let g = graph(&[
			(1, 2),
			(2, 3),
			(3, 4),
			(4, 1),
			(1, 5),
			(5, 6),
			(6, 7),
			(7, 5),
			(6, 8),
			(8, 3),
		]);
		let cut = feedback_vertices(&g);
		assert!(acyclic_without(&g, &cut));
		assert!(cut.len() <= 2);
	}
}
