use anyhow::{Context, Result};

use crate::ast::Ident;
use crate::hir::TypeId;
use crate::mir::{Definition, Expr, Type, TypeKind, TypeMap};
use crate::specializer::{expr::Elaborator, Registry};

impl Definition {
	pub fn exprs(&self) -> Vec<&Expr> {
		let mut out = Vec::new();
		match &self.kind {
			TypeKind::Native(n) => {
				for (_, u) in n
					.specialization
					.iter()
					.chain(&n.virtual_fields)
					.chain(&n.fields)
				{
					out.extend(u.args.iter().map(|(_, e)| e));
				}
				out.extend(n.ref_args.iter().map(|(_, e)| e));
				for item in &n.body {
					out.extend(item.args.iter().map(|(_, e)| e));
				}
			}
			TypeKind::Alias(t) => out.extend(t.args.iter().map(|(_, e)| e)),
		}
		out
	}
}

fn exprs_mut(def: &mut Definition) -> Vec<&mut Expr> {
	let mut out = Vec::new();
	match &mut def.kind {
		TypeKind::Native(n) => {
			for (_, u) in n
				.specialization
				.iter_mut()
				.chain(&mut n.virtual_fields)
				.chain(&mut n.fields)
			{
				out.extend(u.args.iter_mut().map(|(_, e)| e));
			}
			out.extend(n.ref_args.iter_mut().map(|(_, e)| e));
			for item in &mut n.body {
				out.extend(item.args.iter_mut().map(|(_, e)| e));
			}
		}
		TypeKind::Alias(t) => out.extend(t.args.iter_mut().map(|(_, e)| e)),
	}
	out
}

fn expected(types: &TypeMap, def: &Definition) -> Vec<Option<TypeId>> {
	let declared = |list: &[(Ident, TypeId)], name: &Ident| {
		list.iter().find(|(n, _)| n == name).map(|(_, t)| *t)
	};
	let use_args = |u: &Type| -> Vec<Option<TypeId>> {
		u.args
			.iter()
			.map(|(n, _)| declared(&types[u.target].param_types, n))
			.collect()
	};
	let mut out = Vec::new();
	match &def.kind {
		TypeKind::Native(n) => {
			for (_, u) in n
				.specialization
				.iter()
				.chain(&n.virtual_fields)
				.chain(&n.fields)
			{
				out.extend(use_args(u));
			}
			out.extend(n.ref_args.iter().map(|(name, _)| declared(&n.arg_types, name)));
			for item in &n.body {
				out.extend(item.args.iter().map(|_| None));
			}
		}
		TypeKind::Alias(t) => out.extend(use_args(t)),
	}
	out
}

pub fn elaborate(types: &mut TypeMap, reg: &Registry) -> Result<()> {
	let mut done = Vec::new();
	{
		let elab = Elaborator {
			types: &*types,
			reg,
		};
		for (id, def) in &types.0 {
			let mut out = Vec::new();
			for (e, ty) in def.exprs().into_iter().zip(expected(types, def)) {
				let new = match (e, ty) {
					(e, Some(ty)) => Some(elab.coerce(e.clone(), ty)),
					(Expr::Field(_) | Expr::Const(_), None) => None,
					(e, None) => Some(elab.elaborate(e.clone())),
				};
				out.push(
					new.transpose()
						.with_context(|| format!("in {:?}", def.name))?,
				);
			}
			done.push((*id, out));
		}
	}
	for (id, out) in done {
		let def = types.0.get_mut(&id).expect("collected from this map");
		for (slot, new) in exprs_mut(def).into_iter().zip(out) {
			if let Some(new) = new {
				*slot = new;
			}
		}
	}
	Ok(())
}
