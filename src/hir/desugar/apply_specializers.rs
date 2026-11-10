//! Apply specializers desugaring pass
//!
//! Runs after path resolution, before monomorphization. Every use-site reference to
//! a *body-accepting* specializer native invokes the
//! registered [`AstHir`](crate::specializer::AstHir) implementation to project the
//! use-site body into a concrete list of fields, replacing the reference with an
//! inline `Native` whose `fields` are those projected fields. The specializer still
//! owns serialization; the overload variant is recorded on the inline native so
//! generators can dispatch per-overload.
//!
//! Specializer natives *without* a body (e.g. `array`) are not touched here: they
//! are ordinary generic types, monomorphized like any other, with use-site value
//! arguments staying on the reference.

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, Context, Result};

use crate::{
	ast::{Ident, Path},
	hir::{
		BodyDecl, Expr, HirSpecializerCtx, NamespaceItem, NativeType, Reference, ReferenceKind,
		Traverse, TraverseSkip, TypeId, TypeKind, Visitor,
	},
	specializer::{Projection, Registry},
};

use super::super::Namespace;

/// Collect, for the whole tree: every type's absolute path -> id, and which ids are
/// body-accepting specializer natives (id -> (specializer name, overload variant)).
struct Template {
	specializer: Ident,
	variant: Ident,
	decl: BodyDecl,
	param_types: BTreeMap<Ident, TypeKind>,
}

fn collect(
	ns: &Namespace,
	path: &mut Vec<Ident>,
	by_path: &mut HashMap<Path, TypeId>,
	spec: &mut HashMap<TypeId, Template>,
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
					spec.insert(
						t.id(),
						Template {
							specializer: s.clone(),
							variant: variant.clone(),
							decl: decl.clone(),
							param_types: t.param_types.clone(),
						},
					);
				}
			}
		}
	}
}

struct Apply<'a> {
	by_path: &'a HashMap<Path, TypeId>,
	spec: &'a HashMap<TypeId, Template>,
}

impl Visitor for Apply<'_> {
	fn visit_type_kind(&mut self, ty: &mut TypeKind, reg: &Registry) -> Result<()> {
		let TypeKind::Reference(r) = ty else {
			return Ok(());
		};
		let Some(id) = r.ty.resolved() else {
			return Ok(());
		};
		let Some(Template {
			specializer: name,
			variant,
			decl,
			param_types,
		}) = self.spec.get(&id)
		else {
			return Ok(());
		};
		decl.check(&mut r.body)
			.with_context(|| format!("body of {name:?}"))?;
		let specializer = reg
			.asthir
			.get(name.as_str())
			.ok_or_else(|| anyhow!("no specializer implementation for {name:?}"))?;
		let mut arg_types = Vec::new();
		for (n, ty) in param_types {
			let ty = match ty {
				TypeKind::Reference(Reference {
					ty: ReferenceKind::Generic(g),
					..
				}) => r
					.types
					.get(g)
					.cloned()
					.ok_or_else(|| {
						anyhow!(
							"argument {n:?} is a {g:?}: bind `{g:?}: <type>` or give it a typed value like `u8(...)`"
						)
					})?,
				ty => ty.clone(),
			};
			arg_types.push((n.clone(), ty));
		}
		let mut const_args = Vec::new();
		let mut ref_args = Vec::new();
		for (n, a) in &r.args {
			match a {
				Expr::Const(c) if !param_types.contains_key(n) => {
					const_args.push((n.clone(), c.clone()))
				}
				e => ref_args.push((n.clone(), e.clone())),
			}
		}

		let resolve = |n: &str| {
			self.by_path
				.get(&Path {
					absolute: true,
					path: vec![],
					item: Ident::new(n),
				})
				.copied()
		};
		let ctx = HirSpecializerCtx {
			variant,
			body: &r.body,
			types: &r.types,
			args: &r.args,
			resolve: &resolve,
		};
		let Projection {
			fields,
			virtual_fields,
			arg_virtuals,
		} = specializer
			.project(&ctx)
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
			virtual_fields,
			fields,
			accepts_body: None,
			specialization,
			body,
			const_args,
			ref_args,
			arg_types,
			arg_virtuals,
			is_void: false,
			scalar: None,
			attrs: BTreeMap::new(),
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
			parse_ast, BodyField, Expr, HirSpecializerCtx, Namespace, NamespaceItem,
			ReferenceKind, TypeKind, Value,
		},
		specializer::{register_protodef_types, AstHir, Registry},
	};

	struct Probe;
	impl AstHir for Probe {
		fn fields(&self, _ctx: &HirSpecializerCtx) -> Result<Vec<(Ident, TypeKind)>> {
			Ok(vec![])
		}
	}

	const DECL: &str = r#"
		def_native(u8);
		@specializer container
		def_native(container) {
			body {
				item(field) { type(Value); };
				item(virtual_field) { argument(value); type(Value); };
			};
		};
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
		register_protodef_types(&mut reg);
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
							if n.accepts_body.is_none()
								&& n.specializer.as_ref().is_some_and(|s| s.as_str() == "probe")
							{
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
		let Some(Expr::Field(at)) = a.args.get(&Ident::new("at")) else {
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
