//! Generate static version of every generic struct usage

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context as _, Result, anyhow, bail, ensure};

use crate::{
	ast::Ident,
	hir::{
		IdGenerator, Namespace, Reference, ReferenceKind, Traverse, TraverseSkip, Type, TypeId,
		TypeKind, Visitor, id_generator::PendingTypeId,
	},
	specializer::Registry,
};

struct HasGenericReferences<'t>(&'t mut bool);
impl Visitor for HasGenericReferences<'_> {
	fn visit_reference(&mut self, value: &mut Reference, _reg: &Registry) -> Result<()> {
		*self.0 |= value.ty.is_generic(); // || !value.types.is_empty();
		Ok(())
	}
}

/// Collect the params a generic binding captures from its use-site scope. After the
/// pre-mono extraction pass, every scope-crossing reference inside a binding is
/// `$param`-rooted (params of the def the use-site lives in); planting the binding
/// into the monomorphized def closure-converts them: the mono gains the params, the
/// use-site reference passes them as arguments.
struct CollectCaptures<'t>(&'t mut BTreeSet<Ident>);
impl Visitor for CollectCaptures<'_> {
	fn visit_field_ref(&mut self, r: &mut crate::ast::FieldRef) -> Result<()> {
		ensure!(
			r.up == 0,
			"scope-crossing reference in a generic binding survived extraction: {r:?}"
		);
		if let Some(crate::ast::FieldRefSegment::Arg(name)) = r.path.0.first() {
			self.0.insert(name.clone());
		}
		Ok(())
	}
}

type Monomorphizations =
	BTreeMap<(TypeId, BTreeMap<Ident, TypeKind>), (PendingTypeId, String, BTreeSet<Ident>)>;

struct FindGenericTypeReferences<'t> {
	id: IdGenerator,
	generic_types: &'t HashMap<TypeId, String>,
	/// generic types that are natives: unbound generics are allowed there, so even a
	/// reference binding nothing still monomorphizes (to a concrete native instance)
	natives: &'t HashSet<TypeId>,
	monos: &'t mut Monomorphizations,
	found: &'t mut bool,
}
impl Visitor for FindGenericTypeReferences<'_> {
	fn visit_reference(&mut self, value: &mut Reference, reg: &Registry) -> Result<()> {
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
			reg,
		)?;
		if not_resolved_yet {
			return Ok(());
		}

		let mut usage: BTreeMap<Ident, TypeKind> = BTreeMap::new();
		let mut captured = BTreeSet::new();
		for (n, t) in &value.types {
			let mut t = t.clone();
			t.traverse(
				&mut CollectCaptures(&mut captured),
				TraverseSkip::empty(),
				reg,
			)
			.with_context(|| format!("generic binding {n:?}"))?;
			usage.insert(n.clone(), t);
		}

		let pass_captured = |value: &mut Reference, captured: &BTreeSet<Ident>| {
			for name in captured {
				value.args.push((
					name.clone(),
					crate::hir::Expr::Field(crate::ast::FieldRef {
						up: 0,
						path: crate::ast::BaseFieldRef(vec![crate::ast::FieldRefSegment::Arg(
							name.clone(),
						)]),
					}),
				));
			}
		};

		if let Some((monoed, _, captured)) = self.monos.get(&(ty, usage.clone())) {
			if value.ty != ReferenceKind::Resolved(monoed.id()) {
				*self.found = true;
				value.ty = ReferenceKind::Resolved(monoed.id());
				value.types = BTreeMap::new();
				pass_captured(value, &captured.clone());
			}
		} else if !usage.is_empty() || self.natives.contains(&ty) {
			let pending = self.id.next_pending();
			value.ty = ReferenceKind::Resolved(pending.id());
			value.types = BTreeMap::new();
			let name = format!("mono of {value:?}");
			pass_captured(value, &captured);
			self.monos.insert((ty, usage), (pending, name, captured));
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
	fn visit_type_kind(&mut self, tk: &mut TypeKind, _reg: &Registry) -> Result<()> {
		let TypeKind::Reference(re) = tk else {
			return Ok(());
		};
		let ReferenceKind::Generic(generic) = &re.ty else {
			return Ok(());
		};
		if self.still_unset.contains_key(generic) {
			return Ok(());
		}
		let mut resolved = self
			.types
			.get(generic)
			.ok_or_else(|| {
				anyhow!(
					"unknown generic argument: {generic:?} in {:?}, existing: {:?}",
					re,
					self.types
				)
			})?
			.clone();

		match &mut resolved {
			TypeKind::Reference(resolved) => {
				// the binding may itself carry (constant) args (`Value: buffer(count: 256)`);
				// use-site args of the generic reference are appended
				resolved.args.extend(re.args.iter().cloned());
				resolved.types.extend(re.types.clone());
			}
			_ => ensure!(
				re.args.is_empty() && re.types.is_empty(),
				"cannot apply arguments to non-reference generic binding {generic:?}"
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
	fn visit_type(&mut self, ty: &mut Type, reg: &Registry) -> Result<()> {
		for ((_, otypes), (pending, pending_name, captured)) in
			self.monos.extract_if(.., |(m, _), _| &ty.id() == m)
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
				reg,
			)?;
			// Closure-converted captures become params of the mono; use-sites pass
			// them as arguments (added in FindGenericTypeReferences).
			let captured_params = captured.into_iter().collect::<Vec<_>>();
			if let TypeKind::Native(native) = &mut kind {
				// Natives may leave generics unbound (opaque to the compiler, the
				// generator decides): prune them, absent entry = unbound. The mono is
				// fully concrete, so it keeps the native's declared params and no
				// generic set.
				native
					.specialization
					.retain(|(_, v)| !matches!(v, TypeKind::Reference(r) if r.ty.is_generic()));
				let mut params = ty.params.clone();
				params.extend(captured_params);
				self.created
					.push(Type::new(pending, params, kind, BTreeMap::new()));
			} else {
				if !still_unset.is_empty() {
					self.generic_types
						.insert(pending.id(), format!("{pending_name} {types:?}"));
				}
				self.created
					.push(Type::new(pending, captured_params, kind, still_unset));
			}
			*self.created_new = true;
		}
		Ok(())
	}
}

struct FindGenericTypes<'t>(&'t mut HashMap<TypeId, String>, &'t mut HashSet<TypeId>);
impl Visitor for FindGenericTypes<'_> {
	fn visit_type(&mut self, ty: &mut Type, _reg: &Registry) -> Result<()> {
		if ty.types.is_empty() {
			return Ok(());
		}
		self.0.insert(ty.id(), "unknown".to_owned());
		if matches!(ty.kind, TypeKind::Native(_)) {
			self.1.insert(ty.id());
		}
		Ok(())
	}
}

struct EnsureNoReferencesTo<'t>(&'t HashMap<TypeId, String>);
impl Visitor for EnsureNoReferencesTo<'_> {
	fn visit_reference(&mut self, re: &mut Reference, _reg: &Registry) -> Result<()> {
		let Some(r) = re.ty.resolved() else {
			return Ok(());
		};
		if let Some(name) = self.0.get(&r) {
			bail!("failed to resolve generic type arguments, <{name}> is missing");
		};
		Ok(())
	}
}

pub fn monomorphize(id: IdGenerator, ir: &mut Namespace, reg: &Registry) -> Result<()> {
	let mut generic_types = HashMap::new();
	let mut natives = HashSet::new();
	ir.traverse(
		&mut FindGenericTypes(&mut generic_types, &mut natives),
		TraverseSkip::empty(),
		reg,
	)?;

	let mut monos = Monomorphizations::new();
	loop {
		let mut changed = false;
		ir.traverse(
			&mut FindGenericTypeReferences {
				id: id.clone(),
				generic_types: &generic_types,
				natives: &natives,
				monos: &mut monos,
				found: &mut changed,
			},
			TraverseSkip::SKIP_GENERIC_TYPES | TraverseSkip::DFS,
			reg,
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
				reg,
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
		reg,
	)?;
	ir.retain_types(|t| t.types.is_empty());

	Ok(())
}
