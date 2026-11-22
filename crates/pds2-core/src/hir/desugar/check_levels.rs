use anyhow::{Context, Result, bail, ensure};

use crate::{
	ast::{FieldRef, FieldRefSegment, Ident},
	hir::{BodyField, Expr, Namespace, NamespaceItem, TypeKind},
};

fn check_ref(r: &FieldRef, depth: usize, use_site: bool) -> Result<()> {
	let Some(target) = depth.checked_sub(r.up) else {
		bail!(
			"{r:?} goes up {} levels, its item is only {depth} deep in the def",
			r.up
		);
	};
	match r.path.0.first() {
		Some(FieldRefSegment::Arg(_)) => ensure!(
			target == 0,
			"{r:?} names an argument of an item {target} deep in the def, only the def itself has arguments"
		),
		_ => ensure!(
			!use_site || r.up > 0,
			"{r:?} is an argument of its item and points at that item itself"
		),
	}
	Ok(())
}

fn check_expr(e: &Expr, depth: usize, use_site: bool) -> Result<()> {
	match e {
		Expr::Field(r) => check_ref(r, depth, use_site),
		Expr::Const(_) => Ok(()),
		Expr::Binary(_, a, b) => {
			check_expr(a, depth, use_site)?;
			check_expr(b, depth, use_site)
		}
		Expr::Cast(e, _) | Expr::Typed(e, _) => check_expr(e, depth, use_site),
		Expr::Record(fields) => fields
			.iter()
			.try_for_each(|(_, e)| check_expr(e, depth, use_site)),
		Expr::List(items) | Expr::Call(_, items) => items
			.iter()
			.try_for_each(|e| check_expr(e, depth, use_site)),
	}
}

fn check_body(body: &[BodyField], depth: usize) -> Result<()> {
	for f in body {
		for (n, a) in &f.args {
			check_expr(a, depth, false).with_context(|| format!("body item argument {n:?}"))?;
		}
		if let Some(t) = &f.ty {
			check_kind(t, depth + 1)
				.with_context(|| format!("{:?}", f.name.as_ref().unwrap_or(&f.kind)))?;
		}
	}
	Ok(())
}

fn check_nested(items: &[(Ident, TypeKind)], depth: usize) -> Result<()> {
	for (n, t) in items {
		check_kind(t, depth + 1).with_context(|| format!("{n:?}"))?;
	}
	Ok(())
}

fn check_kind(kind: &TypeKind, depth: usize) -> Result<()> {
	match kind {
		TypeKind::Reference(r) => {
			for (n, a) in &r.args {
				check_expr(a, depth, true).with_context(|| format!("argument {n:?}"))?;
			}
			for (n, t) in &r.types {
				check_kind(t, depth + 1).with_context(|| format!("type {n:?}"))?;
			}
			check_body(&r.body, depth)
		}
		TypeKind::Native(n) => {
			for (name, a) in &n.ref_args {
				check_expr(a, depth, true).with_context(|| format!("argument {name:?}"))?;
			}
			check_body(&n.body, depth)?;
			check_nested(&n.fields, depth)?;
			check_nested(&n.virtual_fields, depth)?;
			check_nested(&n.specialization, depth)
		}
	}
}

fn check_ns(ns: &Namespace, path: &mut Vec<Ident>) -> Result<()> {
	for (name, item) in &ns.items {
		path.push(name.clone());
		match item {
			NamespaceItem::Inner(inner) => check_ns(inner, path)?,
			NamespaceItem::Type(t) => {
				if !matches!(&t.kind, TypeKind::Native(n) if n.is_template()) {
					check_kind(&t.kind, 0).with_context(|| {
						format!(
							"in {}",
							path.iter()
								.map(Ident::as_str)
								.collect::<Vec<_>>()
								.join("::")
						)
					})?;
				}
			}
		}
		path.pop();
	}
	Ok(())
}

pub fn check_levels(ns: &Namespace) -> Result<()> {
	check_ns(ns, &mut Vec::new())
}
