//! Replaces trivial aliased types
//!
//! ```rust
//! def(myu8) => u8;
//!
//! def(root) => container {
//!     field(v) => myu8;
//! };
//! =>
//! def(root) => container {
//!     field(v) => u8;
//! };
//! ```

use std::collections::BTreeMap;

use crate::ast::{FieldRefSegment, Ident};
use crate::hir::{
	Expr, Namespace, Reference, ReferenceKind, Traverse, TraverseSkip, Type, TypeId,
	TypeKind, Visitor,
};
use crate::specializer::Registry;
use anyhow::Result;

#[derive(Debug)]
struct Alias {
	target: TypeId,
	/// target argument name -> how to build it from the use-site arguments
	remap_arguments: BTreeMap<Ident, Expr>,
}

fn substitute(template: &Expr, supplied: &[(Ident, Expr)]) -> Expr {
	match template {
		Expr::Field(r) => {
			assert_eq!(r.up, 0);
			let mut suffix = r.path.clone();
			let first_segment = suffix.0.remove(0);
			let FieldRefSegment::Arg(param) = first_segment else {
				panic!("first segment should be arg, got {first_segment:?}")
			};
			let (_, value) = supplied
				.iter()
				.find(|(n, _)| n == &param)
				.unwrap_or_else(|| panic!("alias use-site is missing argument {param:?}"));
			if suffix.0.is_empty() {
				return value.clone();
			}
			let Expr::Field(value) = value else {
				panic!("alias remap through a path needs a field reference")
			};
			let mut value = value.clone();
			value.path.0.extend_from_slice(&suffix.0);
			Expr::Field(value)
		}
		Expr::Const(c) => Expr::Const(c.clone()),
		Expr::Binary(op, a, b) => Expr::Binary(
			*op,
			Box::new(substitute(a, supplied)),
			Box::new(substitute(b, supplied)),
		),
		Expr::Cast(e, t) => Expr::Cast(Box::new(substitute(e, supplied)), t.clone()),
	}
}

struct CollectAliasList<'t> {
	aliases: &'t mut BTreeMap<TypeId, Alias>,
	found_new: bool,
}

impl<'t> CollectAliasList<'t> {
	fn new(aliases: &'t mut BTreeMap<TypeId, Alias>) -> Self {
		Self {
			aliases,
			found_new: false,
		}
	}
}
impl Visitor for CollectAliasList<'_> {
	fn visit_type(&mut self, typ: &mut Type, _reg: &Registry) -> Result<()> {
		// `export;` keeps the def in the output: it stays a named alias to MIR
		// instead of being dissolved into its use sites.
		if typ.export {
			return Ok(());
		}
		if let TypeKind::Reference(Reference {
			ty, args, types, ..
		}) = &mut typ.kind
		{
			assert!(types.is_empty(), "{:?}", typ);
			assert!(ty.is_resolved());
			let resolved = ty.resolved().unwrap();
			let remap_arguments = args
				.iter()
				.map(|(name, value)| (name.clone(), value.clone()))
				.collect();
			if self
				.aliases
				.insert(
					typ.id(),
					Alias {
						target: resolved,
						remap_arguments,
					},
				)
				.is_none()
			{
				self.found_new = true;
			}
		}
		Ok(())
	}
}

struct ReplaceAliasedReferences<'t> {
	aliases: &'t mut BTreeMap<TypeId, Alias>,
	replaced: bool,
}

impl<'t> ReplaceAliasedReferences<'t> {
	fn new(aliases: &'t mut BTreeMap<TypeId, Alias>) -> Self {
		Self {
			aliases,
			replaced: false,
		}
	}
}
impl Visitor for ReplaceAliasedReferences<'_> {
	fn visit_reference(&mut self, value: &mut Reference, _reg: &Registry) -> Result<()> {
		assert!(value.types.is_empty());

		let resolved = value.ty.resolved().unwrap();
		if let Some(alias) = self.aliases.get(&resolved) {
			// Build the target's arguments: each remap names the alias parameter it
			// takes its value from (alias and target parameter names are unrelated).
			let args = alias
				.remap_arguments
				.iter()
				.map(|(target_name, template)| {
					(target_name.clone(), substitute(template, &value.args))
				})
				.collect();
			*value = Reference {
				ty: ReferenceKind::Resolved(alias.target),
				args,
				types: BTreeMap::new(),
				body: vec![],
			};
			self.replaced = true;
		}
		Ok(())
	}
}

pub fn replace_aliases(ir: &mut Namespace, reg: &Registry) -> Result<()> {
	let mut aliases = BTreeMap::new();
	loop {
		let mut had_changed = false;
		{
			let mut visitor = CollectAliasList::new(&mut aliases);
			ir.traverse(&mut visitor, TraverseSkip::SKIP_GENERIC_TYPES, reg)?;
			had_changed |= visitor.found_new;
		}
		{
			let mut visitor = ReplaceAliasedReferences::new(&mut aliases);
			ir.traverse(&mut visitor, TraverseSkip::SKIP_GENERIC_TYPES, reg)?;
			had_changed |= visitor.replaced;
		}
		if !had_changed {
			break;
		}
	}
	ir.retain_types(|t| !aliases.contains_key(&t.id()));
	Ok(())
}
