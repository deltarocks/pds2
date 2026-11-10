use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context as _, Result};

use crate::ast::{FieldRefSegment, Ident};
use crate::hir::TypeId;
use crate::mir::{self, Expr, TypeKind, TypeMap};
use crate::specializer::{expr::Elaborator, Scalar};

/// Sanitize an arbitrary string (`minecraft:foo`) into an identifier, mirroring the
/// upgrade-side variant naming.
pub fn sanitize(key: &str) -> String {
	let mut out: String = key
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() || c == '_' {
				c
			} else {
				'_'
			}
		})
		.collect();
	if out.is_empty() {
		out.push('_');
	}
	if out
		.chars()
		.next()
		.map(|c| c.is_ascii_digit())
		.unwrap_or(false)
	{
		out.insert(0, 'v');
	}
	out
}

pub fn local(name: &Ident) -> String {
	format!("f_{}", name.as_str())
}

#[derive(Default)]
pub struct Elaborated {
	exprs: BTreeMap<TypeId, Vec<Expr>>,
	cast_to: BTreeMap<TypeId, Vec<TypeId>>,
}
impl Elaborated {
	pub fn exprs(&self, id: TypeId) -> &[Expr] {
		self.exprs.get(&id).map_or(&[], Vec::as_slice)
	}
	pub fn cast_to(&self, id: TypeId) -> &[TypeId] {
		self.cast_to.get(&id).map_or(&[], Vec::as_slice)
	}
}

pub fn elaborate(elab: &Elaborator) -> Result<Elaborated> {
	let mut out = Elaborated::default();
	let mut casts: BTreeMap<TypeId, BTreeSet<TypeId>> = BTreeMap::new();
	for (id, def) in &elab.types.0 {
		let exprs = elab
			.exprs(*id)
			.with_context(|| format!("in {:?}", def.name))?;
		for e in &exprs {
			e.walk(&mut |x| {
				if let Expr::Cast(inner, to) = x {
					if let Some(from) = inner.ty() {
						casts.entry(elab.resolve(from)).or_default().insert(*to);
					}
				}
			});
		}
		if !exprs.is_empty() {
			out.exprs.insert(*id, exprs);
		}
	}
	out.cast_to = casts
		.into_iter()
		.map(|(k, v)| (k, v.into_iter().collect()))
		.collect();
	Ok(out)
}

pub fn virtuals_used(input: &TypeMap) -> HashSet<(TypeId, Ident)> {
	let mut out = HashSet::new();
	let mut scan = |e: &Expr| {
		e.walk(&mut |e| {
			let Expr::Field(f) = e else {
				return;
			};
			for item in &f.0 {
				if let FieldRefSegment::Virtual(n) = &item.segment {
					out.insert((item.from.get(), n.clone()));
				}
			}
		})
	};
	for def in input.0.values() {
		let mut uses: Vec<&mir::Type> = Vec::new();
		match &def.kind {
			TypeKind::Native(n) => {
				uses.extend(n.specialization.iter().map(|(_, t)| t));
				uses.extend(n.fields.iter().map(|(_, t)| t));
				uses.extend(n.virtual_fields.iter().map(|(_, t)| t));
				for (_, e) in &n.ref_args {
					scan(e);
				}
				for item in &n.body {
					for (_, a) in &item.args {
						scan(a);
					}
				}
			}
			TypeKind::Alias(t) => uses.push(t),
		}
		for u in uses {
			for (_, arg) in &u.args {
				scan(arg);
			}
		}
	}
	out
}

pub fn scalar_of(input: &TypeMap, id: TypeId) -> Option<Scalar> {
	match &input[id].kind {
		TypeKind::Native(n) => n.scalar.as_ref().and_then(|s| Scalar::parse(s.as_str())),
		TypeKind::Alias(t) => scalar_of(input, t.target),
	}
}
