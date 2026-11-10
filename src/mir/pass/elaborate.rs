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

#[cfg(test)]
mod tests {
	use anyhow::{anyhow, Result};

	use crate::mir::{self, Expr, TypeKind, TypeMap, Value};
	use crate::specializer::{register_protodef_types, Registry};

	const DECL: &str = r#"
		@scalar u8 def_native(u8);
		@scalar u16 def_native(u16);
		@specializer array
		def_native(array) { type(Value); variant(sized) { argument(count); }; };
		@specializer container
		def_native(container) {
			body {
				item(field) { type(Value); };
				item(virtual_field) { argument(value); type(Value); };
			};
		};
		@specializer union
		def_native(union) {
			body { item(variant) { argument(match); type(Value); }; };
			variant(external) { argument(tag); };
		};
		def(Pick) { argument(k) => u8; } => union(tag: ./$k) { variant(one, match: 1) => u8; };
		def(AnyPick) { type(K); argument(k) => K; } => union(tag: ./$k) { variant(one, match: 1) => u8; };
		@specializer terminated
		def_native(terminated) { type(Value); type(End); argument(end) => End; body; };
	"#;

	fn compile(body: &str) -> Result<TypeMap> {
		let mut reg = Registry::default();
		register_protodef_types(&mut reg);
		let src = format!("{DECL} def(T) {{ export; }} => container {{ {body} }};");
		let ast = crate::ast::parse(&src).map_err(|e| anyhow!("{e}"))?;
		mir::from_hir(&crate::hir::parse_ast(ast, &reg)?, &reg)
	}

	fn error(body: &str) -> String {
		format!("{:#}", compile(body).expect_err("should be rejected"))
	}

	fn counts(types: &TypeMap) -> Vec<Expr> {
		types
			.0
			.values()
			.filter_map(|d| match &d.kind {
				TypeKind::Alias(t) => Some(t),
				_ => None,
			})
			.flat_map(|t| t.args.iter().map(|(_, e)| e.clone()))
			.collect()
	}

	#[test]
	fn arithmetic_operands_share_a_type() {
		let err = error("field(a) => u8; field(b) => u16; field(c) => array(count: ../a + ../b) => u8;");
		assert!(err.contains("different types"), "{err}");
		compile("field(a) => u8; field(b) => u16; field(c) => array(count: u16(../a) + ../b) => u8;")
			.unwrap();
	}

	#[test]
	fn casts_need_a_conversion() {
		let err = error("field(a) => u8; field(c) => array(count: Pick(../a)) => u8;");
		assert!(err.contains("can't be cast"), "{err}");
	}

	#[test]
	fn typed_arguments_construct_constants_and_check_fields() {
		compile("field(p) => Pick(k: 1);").unwrap();
		let err = error("field(k) => u16; field(p) => Pick(k: ../k);");
		assert!(err.contains("expected u8, got u16"), "{err}");
	}

	#[test]
	fn generic_argument_types_are_inferred_from_typed_values() {
		compile("field(w) => terminated(end: u8(0)) => u8;").unwrap();
		compile("field(w) => terminated(End: u8, end: 0) => u8;").unwrap();
		compile("field(p) => AnyPick(k: u16(1));").unwrap();
		let err = error("field(w) => terminated(end: 0) => u8;");
		assert!(err.contains("bind `End: <type>`"), "{err}");
		let err = error("field(w) => terminated(End: u8, end: u16(0)) => u8;");
		assert!(err.contains("expected u8, got u16"), "{err}");
	}

	#[test]
	fn constants_fold() {
		let types = compile("field(c) => array(count: 2 * 3 + 1) => u8;").unwrap();
		let folded = counts(&types)
			.into_iter()
			.chain(types.0.values().flat_map(|d| d.exprs().into_iter().cloned()))
			.any(|e| matches!(e, Expr::Const(Value::Signed(7))));
		assert!(folded);
	}
}
