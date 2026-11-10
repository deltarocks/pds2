use pds2_core::specializer::expr::Elaborator;
use pds2_core::mir::{self, Construction, Expr, TypeKind, Value};
use pds2_specializer_protodef::union;

const SRC: &str = r#"
	@scalar u8 def_native(u8);
	def_native(void) { is_void; };
	@specializer container
	def_native(container) {
		body {
			item(field) { type(Value); };
			item(virtual_field) { argument(value); type(Value); };
		};
	};
	@specializer union
	def_native(union) {
		body {
			item(variant) { argument(match); type(Value) => void; };
			item(default) { anonymous; type(Value) => void; };
		};
		variant(external) { argument(tag); };
		variant(inline) { type(Tag); };
	};
	def(Kind) => union(Tag: u8) {
		variant(a, match: 1);
		variant(b, match: 2) => u8;
		default;
	};
	def(T) => container {
		field(k) => Kind;
		field(v) => union(tag: ../k) {
			variant(x, match: "a");
			variant(y, match: 2);
			variant(z, match: 7);
			variant(w, match: 1);
		};
	};
"#;

fn literal(e: &Expr) -> Option<i128> {
	match e {
		Expr::Construct(_, Construction::Literal(Value::Signed(n))) => Some(*n),
		_ => None,
	}
}

#[test]
fn union_tags_compare_by_construction_or_cast() {
	let reg = pds2::registry();
	let ast = pds2_core::ast::parse(SRC).unwrap();
	let types = mir::from_hir(&pds2_core::hir::parse_ast(ast, &reg).unwrap(), &reg).unwrap();
	let elab = Elaborator {
		types: &types,
		reg: &reg,
	};
	let (id, _) = types
		.0
		.iter()
		.find(|(_, d)| {
			matches!(&d.kind, TypeKind::Native(n) if union::view(n).is_some_and(|u| matches!(u.tag, union::TagView::External(_))))
		})
		.unwrap();
	let exprs = elab.exprs(*id).unwrap();
	let [x, y, z, w] = exprs.as_slice() else {
		panic!("{exprs:?}");
	};
	let eq = |e: &Expr| match e {
		Expr::Eq(a, b) => Some((a.as_ref().clone(), b.as_ref().clone())),
		_ => None,
	};

	let (lhs, rhs) = eq(x).unwrap();
	assert!(matches!(lhs, Expr::Var(..)), "{x:?}");
	assert!(
		matches!(&rhs, Expr::Construct(_, Construction::Variant(n, p)) if n.as_str() == "a" && p.is_empty()),
		"{x:?}"
	);

	let (lhs, rhs) = eq(y).unwrap();
	assert!(matches!(lhs, Expr::Cast(..)), "{y:?}");
	assert_eq!(literal(&rhs), Some(2), "{y:?}");

	let (_, rhs) = eq(z).unwrap();
	let Expr::Construct(_, Construction::Variant(n, payload)) = &rhs else {
		panic!("{z:?}");
	};
	assert_eq!(n.as_str(), "default");
	assert_eq!(payload.iter().map(literal).collect::<Vec<_>>(), [Some(7)]);

	let (lhs, rhs) = eq(w).unwrap();
	assert!(matches!(lhs, Expr::Var(..)), "{w:?}");
	assert!(
		matches!(&rhs, Expr::Construct(_, Construction::Variant(n, _)) if n.as_str() == "a"),
		"{w:?}"
	);
}
