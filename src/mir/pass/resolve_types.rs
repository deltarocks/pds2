use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, ensure, Context, Result};

use crate::{
	ast::{FieldRefSegment, Ident},
	hir::TypeId,
	mir::{self, Definition, Field, NativeType, TypeMap},
};

#[derive(Default, Debug)]
struct TypeUsageCollector {
	data: HashMap<TypeId, HashMap<Ident, HashSet<TypeId>>>,
	has_invalid: bool,
	has_valid: bool,
}
impl TypeUsageCollector {
	fn used(&mut self, id: TypeId, name: &Ident, ty: TypeId) {
		if !ty.is_valid() {
			self.has_invalid = true;
			return;
		}
		eprintln!("Resolving {name:?} to {ty:?}");
		let args = self.data.entry(id).or_default();
		let possible = args.entry(name.clone()).or_default();
		if !possible.contains(&ty) {
			self.has_valid = true;
			possible.insert(ty);
		}
	}
	fn reset_validity(&mut self) {
		self.has_invalid = false;
		self.has_valid = false;
	}
	fn find_type(&self, id: TypeId, name: &Ident) -> Result<TypeId> {
		let possible = self.data.get(&id).and_then(|args| args.get(name));
		if possible.is_none() {
			return Ok(TypeId::default());
		}
		let possible = possible.unwrap();
		if possible.is_empty() {
			return Ok(TypeId::default());
		} else if possible.len() != 1 {
			// TODO: conflict resolution/casting
			bail!("type conflict {:?}", possible);
		}
		Ok(possible.iter().next().cloned().unwrap())
	}
}

impl Definition {
	fn resolve_types(
		&self,
		self_id: TypeId,
		types: &TypeMap,
		out: &mut TypeUsageCollector,
	) -> Result<()> {
		match &self.kind {
			mir::TypeKind::Container { fields, .. } => {
				for field in fields.iter() {
					let r = &field.ty;
					for (name, ty) in &r.args {
						out.used(r.target, name, Self::resolve_ref(self_id, types, ty)?);
					}
					if let Some(from) = &field.load_from {
						Self::resolve_ref(self_id, types, from)
							.with_context(|| format!("failed to resolve {:?}", from))?;
					}
				}
			}
			mir::TypeKind::FixedSizeArray { t, .. } => {
				for (name, ty) in &t.args {
					out.used(t.target, name, Self::resolve_ref(self_id, types, ty)?);
				}
			}
			mir::TypeKind::DynamicArray { size_ref, t, .. } => {
				Self::resolve_ref(self_id, types, size_ref)?;
				for (name, ty) in &t.args {
					out.used(t.target, name, Self::resolve_ref(self_id, types, ty)?);
				}
			}
			mir::TypeKind::Union {
				tag,
				matches,
				default,
			} => {
				Self::resolve_ref(self_id, types, tag)?;
				for (_, _, ty) in matches {
					for (name, r) in &ty.args {
						out.used(ty.target, name, Self::resolve_ref(self_id, types, r)?)
					}
				}
				if let Some(default) = default {
					for (name, ty) in &default.1.args {
						out.used(
							default.1.target,
							name,
							Self::resolve_ref(self_id, types, ty)?,
						)
					}
				}
			}
			mir::TypeKind::Native(_native) => {
				// TODO: walk non-native part of native types
			}
			mir::TypeKind::Void => {}
		};
		Ok(())
	}

	fn resolve_ref(from: TypeId, types: &TypeMap, r: &Field) -> Result<TypeId> {
		let mut cur = from;
		for (i, item) in r.0.iter().enumerate() {
			if item.from.get().is_valid() {
				assert_eq!(item.from.get(), cur);
			} else {
				item.from.set(cur);
			}
			match &item.segment {
				FieldRefSegment::Arg(name) => {
					assert_eq!(i, 0);
					let id = types[from]
						.params
						.iter()
						.find(|f| &f.0 == name)
						.ok_or_else(|| {
							anyhow!(
								"unknown argument: {:?}, defined arguments: {:?}",
								name,
								types[from].params
							)
						})?
						.1;
					if !id.is_valid() {
						return Ok(id);
					}
					cur = id;
					if item.to.get().is_valid() {
						assert!(item.to.get() == cur);
					} else {
						item.to.set(cur);
					}
				}
				FieldRefSegment::Down { .. } | FieldRefSegment::Virtual(_) => {
					cur = types[cur].resolve_ref_single(item.segment.clone(), types)?;
					if item.to.get().is_valid() {
						assert!(item.to.get() == cur);
					} else {
						item.to.set(cur);
					}
				}
			}
		}
		Ok(cur)
	}

	fn resolve_ref_single(&self, segment: FieldRefSegment, types: &TypeMap) -> Result<TypeId> {
		Ok(match (segment, &self.kind) {
			(
				segment @ (FieldRefSegment::Virtual(_)
				| FieldRefSegment::Down {
					ignore_delegation: false,
					..
				}),
				mir::TypeKind::Container {
					delegate_all: Some(to),
					fields,
					..
				},
			) => {
				let target = fields
					.iter()
					.find(|f| &f.name == to)
					.ok_or_else(|| anyhow!("delegate_all field not found: {:?}", to))?
					.ty
					.target;
				types[target]
					.resolve_ref_single(segment, types)
					.with_context(|| format!("failed to resolve delegated to {:?}", to))?
			}
			(FieldRefSegment::Down { to, .. }, mir::TypeKind::Container { fields, .. }) => {
				fields
					.iter()
					.find(|f| f.name == to)
					.ok_or_else(|| anyhow!("field not found: {:?}", to))?
					.ty
					.target
			}
			(FieldRefSegment::Virtual(name), mir::TypeKind::DynamicArray { .. })
				if name.as_str() == "size" =>
			{
				let ty = types
					.0
					.iter()
					.find(|t| t.1.name.item == Ident::new("usize"));
				ensure!(ty.is_some(), "expected to find defined usize");

				*ty.unwrap().0
			}
			(
				FieldRefSegment::Virtual(name),
				mir::TypeKind::Native(NativeType {
					virtual_fields,
					specializer: _,
					specialization: _,
				}),
			) => {
				let (_, ty) = virtual_fields
					.iter()
					.find(|(cname, _)| cname == &name)
					.ok_or_else(|| {
						anyhow!("field not found: {name:?}, existing fields: {virtual_fields:?}")
					})?;
				ty.target
			}
			(s, t) => bail!("type {:?} has no fields, but referenced {:?}", t, s),
		})
	}

	fn propogate_types(&mut self, i: TypeId, collector: &TypeUsageCollector) -> Result<()> {
		for (name, param) in self.params.iter_mut() {
			dbg!(&name);
			*param = dbg!(collector.find_type(i, name)?);
		}
		Ok(())
	}

	fn assert_no_unresolved(&self) -> Result<()> {
		for (n, par) in self.params.iter() {
			ensure!(
				par.is_valid(),
				"parameter type was not resolved after all: {n:?}, all params: {:?}",
				self.params
			);
		}
		Ok(())
	}
}

pub fn resolve_types(types: &mut TypeMap) -> Result<()> {
	let mut collector = TypeUsageCollector::default();

	loop {
		println!("resolving types");
		collector.reset_validity();
		for (i, d) in types.0.iter() {
			d.resolve_types(*i, types, &mut collector)?;
		}
		if collector.has_invalid && !collector.has_valid {
			// TODO: print unresolved
			bail!("type resolution stuck");
		} else if !collector.has_invalid && !collector.has_valid {
			break;
		}
		for (i, d) in types.0.iter_mut() {
			d.propogate_types(*i, &collector)?;
		}
	}

	for (_, d) in types.0.iter() {
		d.assert_no_unresolved()
			.with_context(|| format!("in {:?}", d.name))?;
	}

	Ok(())
}
