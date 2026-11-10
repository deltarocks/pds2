//! Apply specializers desugaring pass
//!
//! Runs after path resolution, before monomorphization. Every use-site reference to
//! a *body-accepting* specializer native (`body;` in the `def_native`) invokes the
//! registered [`AstHir`](crate::specializer::AstHir) implementation to project the
//! use-site body into a concrete list of fields, replacing the reference with an
//! inline `Native` whose `fields` are those projected fields. The specializer still
//! owns serialization; the overload variant is recorded on the inline native so
//! generators can dispatch per-overload.
//!
//! Specializer natives *without* a body (e.g. `array`) are not touched here: they
//! are ordinary generic types, monomorphized like any other, with use-site value
//! arguments staying on the reference.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};

use crate::{
	ast::{Ident, Path},
	hir::{
		BodyDecl, HirSpecializerCtx, NamespaceItem, NativeType, Traverse, TraverseSkip, TypeId,
		TypeKind, Visitor,
	},
	specializer::Registry,
};

use super::super::Namespace;

/// Collect, for the whole tree: every type's absolute path -> id, and which ids are
/// body-accepting specializer natives (id -> (specializer name, overload variant)).
fn collect(
	ns: &Namespace,
	path: &mut Vec<Ident>,
	by_path: &mut HashMap<Path, TypeId>,
	spec: &mut HashMap<TypeId, (Ident, Ident, BodyDecl)>,
) {
	for (name, item) in &ns.items {
		match item {
			NamespaceItem::Inner(inner) => {
				path.push(name.clone());
				collect(inner, path, by_path, spec);
				path.pop();
			}
			NamespaceItem::Type(t) => {
				by_path.insert(
					Path {
						absolute: true,
						path: path.clone(),
						item: name.clone(),
					},
					t.id(),
				);
				if let TypeKind::Native(NativeType {
					specializer: Some(s),
					variant,
					accepts_body: Some(decl),
					..
				}) = &t.kind
				{
					spec.insert(t.id(), (s.clone(), variant.clone(), decl.clone()));
				}
			}
		}
	}
}

struct Apply<'a> {
	by_path: &'a HashMap<Path, TypeId>,
	spec: &'a HashMap<TypeId, (Ident, Ident, BodyDecl)>,
}

impl Visitor for Apply<'_> {
	fn visit_type_kind(&mut self, ty: &mut TypeKind, reg: &Registry) -> Result<()> {
		let TypeKind::Reference(r) = ty else {
			return Ok(());
		};
		let Some(id) = r.ty.resolved() else {
			return Ok(());
		};
		let Some((name, variant, decl)) = self.spec.get(&id) else {
			return Ok(());
		};
		decl.check(&mut r.body)
			.with_context(|| format!("body of {name:?}"))?;
		let specializer = reg
			.asthir
			.get(name.as_str())
			.ok_or_else(|| anyhow!("no specializer implementation for {name:?}"))?;
		// Constant configuration arguments move onto the inline native; field
		// references would escape the use-site scope once the reference is gone.
		let const_args = r
			.args
			.iter()
			.map(|(n, a)| match a {
				crate::hir::FieldRefOrConst::Const(c) => Ok((n.clone(), c.clone())),
				crate::hir::FieldRefOrConst::FieldRef(f) => Err(anyhow!(
					"body-accepting native cannot take a field-reference argument: {n:?} = {f:?}"
				)),
			})
			.collect::<Result<Vec<_>>>()?;

		let resolve = |n: &str| {
			self.by_path
				.get(&Path {
					absolute: true,
					path: vec![],
					item: Ident::new(n),
				})
				.copied()
		};
		let fields = specializer
			.fields(&HirSpecializerCtx {
				variant,
				body: &r.body,
				types: &r.types,
				args: &r.args,
				resolve: &resolve,
			})
			.with_context(|| format!("specializer {name:?}"))?;

		let specialization = r
			.types
			.iter()
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		// keep the consumed body around: codegen needs its metadata (sizes, flags)
		let body = std::mem::take(&mut r.body);
		*ty = TypeKind::Native(NativeType {
			specializer: Some(name.clone()),
			variant: variant.clone(),
			virtual_fields: vec![],
			fields,
			accepts_body: None,
			specialization,
			body,
			const_args,
			is_void: false,
			scalar: None,
		});
		Ok(())
	}
}

pub fn apply_specializers(ns: &mut Namespace, reg: &Registry) -> Result<()> {
	let mut by_path = HashMap::new();
	let mut spec = HashMap::new();
	collect(ns, &mut Vec::new(), &mut by_path, &mut spec);

	if spec.is_empty() {
		return Ok(());
	}

	ns.traverse(
		&mut Apply {
			by_path: &by_path,
			spec: &spec,
		},
		TraverseSkip::DFS,
		reg,
	)
}

#[cfg(test)]
mod tests {
	use anyhow::Result;

	use crate::{
		ast::{FieldRefSegment, Ident},
		hir::{
			parse_ast, BodyField, FieldRefOrConst, HirSpecializerCtx, Namespace, NamespaceItem,
			ReferenceKind, TypeKind, Value,
		},
		specializer::{AstHir, Registry},
	};

	struct Probe;
	impl AstHir for Probe {
		fn fields(&self, _ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, TypeKind)>> {
			Ok(vec![])
		}
	}

	const DECL: &str = r#"
		def_native(u8);
		@specializer probe
		def_native(probe) {
			body {
				item(field) {
					argument(size) => 8;
					argument(at);
					type(Value) => u8;
				};
				item(flag);
				item(default) {
					anonymous;
					type(Value);
				};
			};
		};
	"#;

	fn compile(body: &str) -> Result<Namespace> {
		let mut reg = Registry::default();
		reg.register_asthir("probe", Probe);
		let src = format!(
			"{DECL} def(T) => container {{ field(n) => u8; field(p) => probe {{ {body} }}; }};"
		);
		let ast = crate::ast::parse(&src).map_err(|e| anyhow::anyhow!("{e}"))?;
		parse_ast(ast, &reg)
	}

	fn probe_body(ns: &Namespace) -> Vec<BodyField> {
		fn walk(ns: &Namespace, out: &mut Vec<BodyField>) {
			for item in ns.items.values() {
				match item {
					NamespaceItem::Inner(inner) => walk(inner, out),
					NamespaceItem::Type(t) => {
						if let TypeKind::Native(n) = &t.kind {
							if n.accepts_body.is_none() && !n.body.is_empty() {
								out.extend(n.body.iter().cloned());
							}
						}
					}
				}
			}
		}
		let mut out = Vec::new();
		walk(ns, &mut out);
		out
	}

	fn error(body: &str) -> String {
		format!("{:#}", compile(body).expect_err("body should be rejected"))
	}

	#[test]
	fn body_defaults_are_filled_and_refs_captured() {
		let ns = compile("field(a, at: ../n); field(b, at: 1, size: 3) => u8; flag(c); default => u8;")
			.unwrap();
		let body = probe_body(&ns);
		let [a, b, c, d] = body.as_slice() else {
			panic!("unexpected body {body:?}");
		};
		let is_u8 = |f: &BodyField| matches!(&f.ty, Some(TypeKind::Reference(r)) if matches!(r.ty, ReferenceKind::Resolved(_)));

		assert_eq!(a.constant("size"), Some(&Value::Number(8)));
		let Some(FieldRefOrConst::FieldRef(at)) = a.args.get(&Ident::new("at")) else {
			panic!("`at` should stay a reference: {a:?}");
		};
		assert_eq!(at.up, 0);
		assert!(matches!(at.path.0.as_slice(), [FieldRefSegment::Arg(_)]), "{at:?}");
		assert!(is_u8(a), "default type should be filled: {a:?}");

		assert_eq!(b.constant("size"), Some(&Value::Number(3)));
		assert_eq!(b.constant("at"), Some(&Value::Number(1)));
		assert!(is_u8(b));

		assert_eq!(c.name, Some(Ident::new("c")));
		assert!(c.ty.is_none());

		assert_eq!(d.kind, Ident::new("default"));
		assert_eq!(d.name, None);
		assert!(is_u8(d));
	}

	#[test]
	fn body_items_are_checked_against_declaration() {
		for (body, expected) in [
			("bogus(x);", "unknown body item bogus"),
			("field(a);", "field needs argument at"),
			("field(a, at: 1, zzz: 2);", "field has no argument zzz"),
			("field(at: 1);", "field needs a name"),
			("default(x) => u8;", "default takes no name"),
			("default => u8; default => u8;", "default given twice"),
			("default;", "default needs `=> type`"),
			("flag(c) => u8;", "flag takes no `=> type`"),
			("flag(a); field(a, at: 1);", "duplicate body item name a"),
			("field(a, at: 1, Size: u8);", "takes its type as `=> type`"),
		] {
			let err = error(body);
			assert!(err.contains(expected), "{body}: {err}");
		}
	}
}
