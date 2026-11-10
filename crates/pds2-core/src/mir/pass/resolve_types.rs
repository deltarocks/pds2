use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{anyhow, bail, ensure, Context, Result};

use crate::{
	ast::{FieldRefSegment, Ident},
	hir::TypeId,
	mir::{self, Definition, Expr, Field, NativeType, TypeMap},
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
			mir::TypeKind::Native(native) => {
				for (_, sub) in native
					.specialization
					.iter()
					.chain(native.virtual_fields.iter())
					.chain(native.fields.iter())
				{
					Self::resolve_type_use(self_id, sub, types, out)?;
				}
				for (n, r) in &native.ref_args {
					Self::resolve_expr(self_id, types, r)
						.with_context(|| format!("argument {n:?}"))?;
				}
				for item in &native.body {
					for (n, a) in &item.args {
						Self::resolve_expr(self_id, types, a).with_context(|| {
							format!("body item {:?} argument {n:?}", item.kind)
						})?;
					}
				}
			}
			mir::TypeKind::Alias(ty) => {
				Self::resolve_type_use(self_id, ty, types, out)?;
			}
		};
		Ok(())
	}

	/// Resolve the argument references of a single type-use (`target` plus the
	/// `args` bound to its params) in `scope`.
	fn resolve_type_use(
		scope: TypeId,
		ty: &mir::Type,
		types: &TypeMap,
		out: &mut TypeUsageCollector,
	) -> Result<()> {
		for (name, arg) in &ty.args {
			if let Some(t) = Self::resolve_expr(scope, types, arg)? {
				out.used(ty.target, name, t);
			}
		}
		Ok(())
	}

	fn resolve_expr(scope: TypeId, types: &TypeMap, e: &Expr) -> Result<Option<TypeId>> {
		Ok(match e {
			Expr::Field(f) => Some(Self::resolve_ref(scope, types, f)?),
			Expr::Const(_) => None,
			Expr::Var(_, t) | Expr::Construct(t, _) => Some(*t),
			Expr::Cast(e, t) => {
				Self::resolve_expr(scope, types, e)?;
				Some(*t)
			}
			Expr::Binary(_, a, b) => {
				let a = Self::resolve_expr(scope, types, a)?;
				let b = Self::resolve_expr(scope, types, b)?;
				a.or(b)
			}
			Expr::Eq(a, b) => {
				Self::resolve_expr(scope, types, a)?;
				Self::resolve_expr(scope, types, b)?;
				None
			}
			Expr::Record(fields) => {
				for (_, e) in fields {
					Self::resolve_expr(scope, types, e)?;
				}
				None
			}
			Expr::List(items) => {
				for e in items {
					Self::resolve_expr(scope, types, e)?;
				}
				None
			}
			Expr::Serialized(e) | Expr::Deserialized(e) => {
				Self::resolve_expr(scope, types, e)?;
				None
			}
		})
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
					cur = types[cur].resolve_ref_single(cur, item.segment.clone(), types)?;
					if !cur.is_valid() {
						return Ok(cur);
					}
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

	fn resolve_ref_single(
		&self,
		self_id: TypeId,
		segment: FieldRefSegment,
		types: &TypeMap,
	) -> Result<TypeId> {
		Ok(match (segment, &self.kind) {
			(FieldRefSegment::Virtual(name), mir::TypeKind::Native(native))
				if native.arg_virtuals.contains(&name) =>
			{
				let (_, r) = native
					.ref_args
					.iter()
					.find(|(n, _)| n == &name)
					.ok_or_else(|| anyhow!("virtual {name:?} has no argument to take its type from"))?;
				Self::resolve_expr(self_id, types, r)?
					.ok_or_else(|| anyhow!("argument {name:?} has no type for virtual {name:?}"))?
			}
			(
				FieldRefSegment::Virtual(name),
				mir::TypeKind::Native(NativeType { virtual_fields, .. }),
			) => {
				let (_, ty) = virtual_fields
					.iter()
					.find(|(cname, _)| cname == &name)
					.ok_or_else(|| {
						anyhow!("field not found: {name:?}, existing fields: {virtual_fields:?}")
					})?;
				ty.target
			}
			(
				FieldRefSegment::Down { to, .. },
				mir::TypeKind::Native(NativeType { fields, .. }),
			) => {
				let (_, ty) = fields
					.iter()
					.find(|(cname, _)| cname == &to)
					.ok_or_else(|| {
						anyhow!("field not found: {to:?}, existing fields: {fields:?}")
					})?;
				ty.target
			}
			(s, t) => bail!("type {:?} has no fields, but referenced {:?}", t, s),
		})
	}

	fn propogate_types(&mut self, i: TypeId, collector: &TypeUsageCollector) -> Result<()> {
		for (name, param) in self.params.iter_mut() {
			*param = match self.param_types.iter().find(|(n, _)| n == name) {
				Some((_, declared)) => *declared,
				None => collector
					.find_type(i, name)
					.with_context(|| format!("argument {name:?} of {}", self.name.readable_name()))?,
			};
		}
		Ok(())
	}

	fn assert_no_unresolved(&self) -> Result<()> {
		// A native's params may legitimately stay uninferred when every use passes a
		// constant (`buffer(count: 256)`): there is no referenced field to take the
		// type from. The generator falls back to a plain integer for them.
		if matches!(self.kind, mir::TypeKind::Native(_)) {
			return Ok(());
		}
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
		collector.reset_validity();
		for (i, d) in types.0.iter() {
			d.resolve_types(*i, types, &mut collector)
				.with_context(|| format!("in {:?}", d.name))?;
		}
		if collector.has_invalid && !collector.has_valid {
			let mut unresolved = Vec::new();
			for (i, d) in types.0.iter() {
				for (name, _) in d.params.iter() {
					if !collector.find_type(*i, name)?.is_valid() {
						unresolved.push(format!("{:?} argument {name:?}", d.name));
					}
				}
			}
			bail!(
				"type resolution stuck; could not infer a type for the following \
				 (most likely generic definitions that are never instantiated): {}",
				unresolved.join(", ")
			);
		} else if !collector.has_invalid && !collector.has_valid {
			break;
		}
		let names: BTreeMap<TypeId, String> = types
			.0
			.iter()
			.map(|(i, d)| (*i, d.name.readable_name()))
			.collect();
		for (i, d) in types.0.iter_mut() {
			d.propogate_types(*i, &collector).map_err(|e| {
				let mut msg = format!("{e:#}");
				for (id, name) in &names {
					msg = msg.replace(&format!("{id:?}"), name);
				}
				anyhow!("{msg}")
			})?;
		}
	}

	for (_, d) in types.0.iter() {
		d.assert_no_unresolved()
			.with_context(|| format!("in {:?}", d.name))?;
	}

	Ok(())
}
