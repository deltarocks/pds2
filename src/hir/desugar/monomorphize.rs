//! Generate static version of every generic struct usage

use std::{collections::HashSet, mem};

use anyhow::{anyhow, bail, Result};

use crate::{
	ast::Ident,
	hir::{
		id_generator::PendingTypeId, ContainerFieldType, IdGenerator, Namespace, NamespaceItem,
		Reference, ReferenceKind, Type, TypeId, TypeKind,
	},
	should_be_desugared,
};

#[derive(Debug)]
struct Monomorphization {
	/// Id if the monomorpized type to be created
	mono_type: PendingTypeId,
	/// Type, to which mono_type should point
	target: TypeId,
	types: Vec<(Ident, TypeKind)>,
	implemented: bool,
}

struct MonoCollector {
	id: IdGenerator,
	generic: HashSet<TypeId>,
	path: Vec<Ident>,
	extracted: Vec<Monomorphization>,
	added: Vec<(Vec<Ident>, Type)>,
}
impl MonoCollector {
	fn push(&mut self, ident: impl Into<Ident>) {
		self.path.push(ident.into());
	}
	fn pop(&mut self) {
		self.path.pop().expect("stack underflow");
	}
	fn collect(&mut self, ty: TypeId, types: Vec<(Ident, TypeKind)>) -> TypeId {
		let (id, pending_id) = self.id.next_pending();

		self.extracted.push(Monomorphization {
			mono_type: pending_id,
			target: ty,
			types,
			implemented: false,
		});
		id
	}
	fn is_generic(&self, ty: TypeId) -> bool {
		self.generic.contains(&ty)
	}
	fn add(&mut self, ty: Type) {
		self.added.push((self.path.clone(), ty));
	}
}

impl TypeKind {
	fn monomorphize(
		&mut self,
		collector: &mut MonoCollector,
		forty: &Vec<(Ident, TypeKind)>,
	) -> Result<()> {
		match self {
			TypeKind::Void => {}
			TypeKind::Container(container) => {
				for (name, field) in container.fields.iter_mut() {
					collector.push(name.clone());
					match field {
						ContainerFieldType::Normal(n) => n.monomorphize(collector, forty)?,
						ContainerFieldType::Virtual(_, t) => {
							t.monomorphize(collector, forty)?;
						}
						ContainerFieldType::Delegate(_) => {}
					}
					collector.pop();
				}
			}
			TypeKind::Reference(Reference { ty, args: _, types }) => {
				if ty.is_generic() {
					let generic = ty.generic().unwrap();
					let r = &forty
						.iter()
						.find(|(n, _)| n == &generic)
						.ok_or_else(|| {
							anyhow!("generic not found: {generic:?}, in scope: {forty:?}")
						})?
						.1;
					r.assert_reference();
					assert!(types.is_empty(), "no HKT for now");
					let TypeKind::Reference(Reference {
						ty: nty,
						args,
						types,
					}) = r else {
						unreachable!()
					};
					assert!(args.is_empty() && types.is_empty());
					*ty = nty.clone();
				}

				if collector.is_generic(ty.resolved().unwrap()) {
					let types = mem::take(types);
					let monoty = collector
						.collect(ty.clone().resolved().expect("paths are resolved"), types);
					*ty = ReferenceKind::Resolved(monoty);
				} else {
					assert!(types.is_empty());
				}
			}
			TypeKind::Native(value) => {
				let crate::hir::NativeType {
					virtual_fields: _,
					specializer: _,
					specialization,
				} = value;
				{
					for (n, k) in specialization.iter_mut() {
						collector.push(n.clone());
						k.monomorphize(collector, forty)?;
						collector.pop();
					}
				}
			}
		}
		Ok(())
	}
}
impl Type {
	fn monomorphize(&mut self, collector: &mut MonoCollector) -> Result<()> {
		for mono in collector
			.extracted
			.drain_filter(|f| f.target == self.id())
			.collect::<Vec<_>>()
		{
			let mut mono_kind = self.kind.clone();
			let mut types = mono.types.clone();
			for (n, t) in self.types.iter() {
				if types.iter().any(|(cn, _)| cn == n) {
					continue;
				}
				if t.is_some() {
					types.push((n.clone(), t.clone().unwrap()));
				} else {
					bail!("missing generic parameter: {:?}, passed: {:?}", n, types)
				}
			}
			mono_kind.monomorphize(collector, &types)?;
			let new_type = Type::new(mono.mono_type, self.params.clone(), mono_kind);
			collector.add(new_type);
		}
		if self.types.is_empty() {
			self.kind.monomorphize(collector, &Vec::new())?;
		}
		Ok(())
	}
}

impl Namespace {
	// Acquire set of types, references to whose should be monomorphised
	fn get_generic_types(&self, out: &mut HashSet<TypeId>) -> bool {
		let mut found = false;
		for (_, item) in self.items.iter() {
			match item {
				NamespaceItem::Inner(ns) => found |= ns.get_generic_types(out),
				NamespaceItem::Type(ty) => {
					if !ty.types.is_empty() && out.insert(ty.id()) {
						found = true
					}
				}
			}
		}
		found
	}
	fn monomorphize(&mut self, collector: &mut MonoCollector) -> Result<()> {
		for (name, item) in self.items.iter_mut() {
			collector.push(name.clone());
			collector.push("mono");
			match item {
				NamespaceItem::Inner(ns) => ns.monomorphize(collector)?,
				NamespaceItem::Type(ty) => {
					ty.monomorphize(collector)?;
				}
			}
			collector.pop();
			collector.pop();
		}
		Ok(())
	}
	fn remove_generic(&mut self) {
		let removed = self
			.items
			.drain_filter(|_, i| match i {
				NamespaceItem::Inner(ns) => {
					ns.remove_generic();
					false
				}
				NamespaceItem::Type(t) => !t.types.is_empty(),
			})
			.count();
		println!("removed {removed} generic types")
	}
}

pub fn monomorphize(id: IdGenerator, ir: &mut Namespace) -> Result<()> {
	let mut collector = MonoCollector {
		id,
		generic: HashSet::new(),
		path: Vec::new(),
		extracted: Vec::new(),
		added: Vec::new(),
	};
	loop {
		ir.get_generic_types(&mut collector.generic);
		ir.monomorphize(&mut collector)?;
		let found = ir.get_generic_types(&mut collector.generic);

		let left_unimplemented = collector
			.extracted
			.iter()
			.filter(|e| !e.implemented)
			.count();
		if left_unimplemented == 0 && !found {
			break;
		}
	}
	for (path, ty) in collector.added {
		let mut name = path
			.iter()
			.map(|c| c.as_str())
			.collect::<Vec<_>>()
			.join("_");
		while ir.items.contains_key(&Ident::new(&name)) {
			name += "_yet_another";
		}
		ir.items
			.insert((&name as &str).into(), NamespaceItem::Type(ty));
	}
	ir.remove_generic();
	Ok(())
	// dbg!(collector.extracted);
}
