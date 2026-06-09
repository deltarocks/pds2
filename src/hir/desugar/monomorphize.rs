//! Generate static version of every generic struct usage

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, bail, ensure, Result};

use crate::{
	ast::Ident,
	hir::{
		id_generator::PendingTypeId, IdGenerator, Namespace, Reference, ReferenceKind, Traverse,
		TraverseSkip, Type, TypeId, TypeKind, Visitor,
	},
};

struct HasGenericReferences<'t>(&'t mut bool);
impl Visitor for HasGenericReferences<'_> {
	fn visit_reference(&mut self, value: &mut Reference) -> Result<()> {
		*self.0 |= value.ty.is_generic(); // || !value.types.is_empty();
		Ok(())
	}
}

type Monomorphizations = BTreeMap<(TypeId, BTreeMap<Ident, TypeKind>), (PendingTypeId, String)>;

struct FindGenericTypeReferences<'t> {
	id: IdGenerator,
	generic_types: &'t HashMap<TypeId, String>,
	monos: &'t mut Monomorphizations,
	found: &'t mut bool,
}
impl Visitor for FindGenericTypeReferences<'_> {
	fn visit_reference(&mut self, value: &mut Reference) -> Result<()> {
		let Some(ty) = value.ty.resolved() else {
			unreachable!()
		};
		if !self.generic_types.contains_key(&ty) {
			ensure!(value.types.is_empty(), "type is not generic");
			return Ok(());
		}

		let mut not_resolved_yet = false;
		value.traverse(
			&mut HasGenericReferences(&mut not_resolved_yet),
			TraverseSkip::empty(),
		)?;
		if not_resolved_yet {
			return Ok(());
		}

		let mut usage: BTreeMap<Ident, TypeKind> = BTreeMap::new();
		for (n, t) in &value.types {
			if let TypeKind::Reference(re) = t {
				ensure!(re.args.is_empty(), "no args for generic");
			}
			usage.insert(n.clone(), t.clone());
		}

		if let Some((monoed, _)) = self.monos.get(&(ty, usage.clone())) {
			if value.ty != ReferenceKind::Resolved(monoed.id()) {
				*self.found = true;
			}
			value.ty = ReferenceKind::Resolved(monoed.id());
			value.types = BTreeMap::new();
		} else if !usage.is_empty() {
			let pending = self.id.next_pending();
			value.ty = ReferenceKind::Resolved(pending.id());
			value.types = BTreeMap::new();
			self.monos
				.insert((ty, usage), (pending, format!("mono of {value:?}")));
			*self.found = true;
		}
		Ok(())
	}
}

struct ReplaceGenericReferencesWithSpecified<'t> {
	types: &'t BTreeMap<Ident, TypeKind>,
	still_unset: &'t BTreeMap<Ident, Option<TypeKind>>,
}
impl Visitor for ReplaceGenericReferencesWithSpecified<'_> {
	fn visit_type_kind(&mut self, tk: &mut TypeKind) -> Result<()> {
		let TypeKind::Reference(re) = tk else {
			return Ok(());
		};
		let ReferenceKind::Generic(gen) = &re.ty else {
			return Ok(());
		};
		if self.still_unset.contains_key(gen) {
			return Ok(());
		}
		let mut resolved = self
			.types
			.get(gen)
			.ok_or_else(|| {
				anyhow!(
					"unknown generic argument: {gen:?} in {:?}, existing: {:?}",
					re,
					self.types
				)
			})?
			.clone();

		match &mut resolved {
			TypeKind::Reference(resolved) => {
				assert!(resolved.args.is_empty());
				resolved.args = re.args.clone();
				resolved.types = re.types.clone();
			}
			_ => ensure!(
				re.args.is_empty() && re.types.is_empty(),
				"cannot apply arguments to non-reference generic binding {gen:?}"
			),
		}

		*tk = resolved;
		Ok(())
	}
}

struct ImplementMonomorphized<'t> {
	generic_types: &'t mut HashMap<TypeId, String>,
	monos: &'t mut Monomorphizations,
	created: &'t mut Vec<Type>,
	created_new: &'t mut bool,
}
impl Visitor for ImplementMonomorphized<'_> {
	fn visit_type(&mut self, ty: &mut Type) -> Result<()> {
		for ((_, otypes), (pending, pending_name)) in
			self.monos.extract_if(|(m, _), _| &ty.id() == m)
		{
			let mut kind = ty.kind.clone();
			let mut types = BTreeMap::new();
			let mut still_unset = BTreeMap::new();
			for (n, default) in &ty.types {
				if let Some(default) = default {
					types.insert(n.clone(), default.clone());
				} else if let Some(provided) = otypes.get(n) {
					types.insert(n.clone(), provided.clone());
				} else {
					still_unset.insert(n.clone(), None);
				}
			}
			for (n, v) in otypes {
				if types.insert(n.clone(), v).is_none() {
					bail!("unknown generic: {n:?}, existing: {types:?}");
				}
			}
			kind.traverse(
				&mut ReplaceGenericReferencesWithSpecified {
					types: &types,
					still_unset: &still_unset,
				},
				TraverseSkip::empty(),
			)?;
			if !still_unset.is_empty() {
				self.generic_types.insert(pending.id(), format!("{pending_name} {types:?}"));
			}
			self.created
				.push(Type::new(pending, Vec::new(), kind, still_unset));
			*self.created_new = true;
		}
		Ok(())
	}
}

struct FindGenericTypes<'t>(&'t mut HashMap<TypeId, String>);
impl Visitor for FindGenericTypes<'_> {
	fn visit_type(&mut self, ty: &mut Type) -> Result<()> {
		if ty.types.is_empty() {
			return Ok(());
		}
		self.0.insert(ty.id(), "unknown".to_owned());
		Ok(())
	}
}

struct EnsureNoReferencesTo<'t>(&'t HashMap<TypeId, String>);
impl Visitor for EnsureNoReferencesTo<'_> {
	fn visit_reference(&mut self, re: &mut Reference) -> Result<()> {
		let Some(r) = re.ty.resolved() else {
			return Ok(());
		};
		if let Some(name) = self.0.get(&r) {
			bail!("failed to resolve generic type arguments, <{name}> is missing");
		};
		Ok(())
	}
}

pub fn monomorphize(id: IdGenerator, ir: &mut Namespace) -> Result<()> {
	let mut generic_types = HashMap::new();
	ir.traverse(
		&mut FindGenericTypes(&mut generic_types),
		TraverseSkip::empty(),
	)?;

	let mut monos = Monomorphizations::new();
	loop {
		let mut changed = false;
		ir.traverse(
			&mut FindGenericTypeReferences {
				id: id.clone(),
				generic_types: &generic_types,
				monos: &mut monos,
				found: &mut changed,
			},
			TraverseSkip::SKIP_GENERIC_TYPES | TraverseSkip::DFS,
		)?;
		ir.derive_types(|t| {
			let mut out = Vec::new();
			t.traverse(
				&mut ImplementMonomorphized {
					generic_types: &mut generic_types,
					monos: &mut monos,
					created: &mut out,
					created_new: &mut changed,
				},
				TraverseSkip::empty(),
			)?;
			Ok(out)
		})?;
		if !changed {
			break;
		}
	}

	ir.traverse(
		&mut EnsureNoReferencesTo(&generic_types),
		TraverseSkip::SKIP_GENERIC_TYPES,
	)?;
	ir.retain_types(|t| t.types.is_empty());

	Ok(())
}
