//! Generate static version of every generic struct usage

use std::mem;

use crate::{
	ast::Ident,
	hir::{
		id_generator::PendingTypeId, ArrayLength, ContainerFieldType, IdGenerator, Namespace,
		NamespaceItem, Type, TypeId, TypeKind, UnionTag,
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
	fn add(&mut self, ty: Type) {
		self.added.push((self.path.clone(), ty));
	}
}

impl TypeKind {
	fn monomorphize(&mut self, collector: &mut MonoCollector, forty: &Vec<(Ident, TypeKind)>) {
		match self {
			TypeKind::Void => {}
			TypeKind::Container(container) => {
				for (name, field) in container.fields.iter_mut() {
					collector.push(name.clone());
					match field {
						ContainerFieldType::Normal(n) => n.monomorphize(collector, forty),
						ContainerFieldType::Virtual(_, t) => {
							t.monomorphize(collector, forty);
						}
						ContainerFieldType::Delegate(d) => {}
					}
					collector.pop();
				}
			}
			TypeKind::Union(u) => {
				match u.tag {
					UnionTag::LoadFrom(_) => {}
					UnionTag::Type(_) => should_be_desugared!(u.tag),
				}
				for (_value, _ident, ty) in u.matches.iter_mut() {
					ty.monomorphize(collector, forty)
				}
			}
			TypeKind::Array(arr) => {
				match arr.length {
					ArrayLength::LoadFrom(_) => {}
					ArrayLength::Fixed(_) => {}
					ArrayLength::Type(_) => should_be_desugared!(arr),
				}
				arr.ty.monomorphize(collector, forty)
			}
			TypeKind::Reference { ty, args: _, types } => {
				if !types.is_empty() {
					let types = mem::take(types);
					let monoty = collector.collect(ty.clone().expect("paths are resolved"), types);
					*ty = Ok(monoty);
				}
			}
			TypeKind::Native(value) => {
				dbg!("mono params", forty);
				match value {
					crate::hir::NativeTypeKind::Int(_) => todo!(),
					crate::hir::NativeTypeKind::Uint(_) => todo!(),
					crate::hir::NativeTypeKind::Opaque {
						virtual_fields: _,
						specializer: _,
						specialization,
					} => {
						assert!(specialization.is_empty(), "should only be visited once");
						*specialization = forty.clone();
					}
					crate::hir::NativeTypeKind::Pending => todo!(),
				}
			}
		}
	}
}
impl Type {
	fn monomorphize(&mut self, collector: &mut MonoCollector) {
		if self.types.is_empty() {
			self.kind.monomorphize(collector, &Vec::new());
		} else {
			for mono in collector
				.extracted
				.drain_filter(|f| f.target == self.id())
				.collect::<Vec<_>>()
			{
				let mut mono_kind = self.kind.clone();
				mono_kind.monomorphize(collector, &mono.types);
				let new_type = Type::new(mono.mono_type, self.params.clone(), mono_kind);
				collector.add(new_type);
			}
		}
	}
}

impl Namespace {
	fn monomorphize(&mut self, collector: &mut MonoCollector) {
		for (name, item) in self.items.iter_mut() {
			collector.push(name.clone());
			collector.push("mono");
			match item {
				NamespaceItem::Inner(ns) => ns.monomorphize(collector),
				NamespaceItem::Type(ty) => {
					ty.monomorphize(collector);
				}
			}
			collector.pop();
			collector.pop();
		}
		// for (path, ty) in collector.extracted {
		// 	let mut name = path
		// 		.iter()
		// 		.map(|c| c.as_str())
		// 		.collect::<Vec<_>>()
		// 		.join("_");
		// 	while self.items.contains_key(&Ident::new(&name)) {
		// 		name += "_yet_another";
		// 	}
		// 	self.items
		// 		.insert((&name as &str).into(), NamespaceItem::Type(ty));
		// }
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

pub fn monomorphize(id: IdGenerator, ir: &mut Namespace) {
	let mut collector = MonoCollector {
		id,
		path: Vec::new(),
		extracted: Vec::new(),
		added: Vec::new(),
	};
	loop {
		ir.monomorphize(&mut collector);

		let left_unimplemented = collector
			.extracted
			.iter()
			.filter(|e| !e.implemented)
			.count();
		if left_unimplemented == 0 {
			break;
		}
		eprintln!("Left to implement: {}", left_unimplemented)
	}
	eprintln!("Adding {:?}", collector.added);
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
	ir.remove_generic()
	// dbg!(collector.extracted);
}
