use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, ensure, Context, Result};

use crate::{
    ast::Ident,
    hir::TypeId,
    mir::{self, Definition, Field},
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
        types: &HashMap<TypeId, Definition>,
        out: &mut TypeUsageCollector,
    ) -> Result<()> {
        match &self.kind {
            mir::TypeKind::Container { fields, .. } => {
                for field in fields.iter() {
                    let r = &field.ty;
                    for (name, ty) in &r.args {
                        out.used(r.target, name, Self::resolve_ref(self_id, types, &ty)?);
                    }
                    if let Some(from) = &field.load_from {
                        Self::resolve_ref(self_id, types, &from)
                            .with_context(|| format!("Resolving {:?}", from))?;
                    }
                }
            }
            mir::TypeKind::FixedSizeArray { t, .. } | mir::TypeKind::DynamicArray { t, .. } => {
                for (name, ty) in &t.args {
                    out.used(t.target, name, Self::resolve_ref(self_id, types, &ty)?);
                }
            }
            mir::TypeKind::Union {
                tag,
                matches,
                default,
            } => {
                for (_, _, ty) in matches {
                    for (name, r) in &ty.args {
                        out.used(ty.target, name, Self::resolve_ref(self_id, types, &r)?)
                    }
                }
                if let Some(default) = default {
                    for (name, ty) in &default.1.args {
                        out.used(
                            default.1.target,
                            name,
                            Self::resolve_ref(self_id, types, &ty)?,
                        )
                    }
                }
            }
            mir::TypeKind::Native(_) => {}
        };
        Ok(())
    }

    fn resolve_ref(from: TypeId, types: &HashMap<TypeId, Definition>, r: &Field) -> Result<TypeId> {
        let mut cur = from;
        for (i, item) in r.0.iter().enumerate() {
            if item.from.get().is_valid() {
                assert_eq!(item.from.get(), cur);
            } else {
                item.from.set(cur);
            }
            match item.kind {
                mir::FieldItemKind::Argument => {
                    let id = types
                        .get(&from)
                        .expect("invalid reference")
                        .params
                        .iter()
                        .find(|f| f.0 == item.name)
                        .ok_or_else(|| anyhow!("unknown argument: {:?}", item.name))?
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
                mir::FieldItemKind::Field => {
                    cur = types
                        .get(&cur)
                        .expect("invalid reference")
                        .resolve_ref_single(item.name.clone())?;
                    if item.to.get().is_valid() {
                        assert!(item.to.get() == cur);
                    } else {
                        item.to.set(cur);
                    }
                }
                mir::FieldItemKind::Virtual => {
                    let ty = types.iter().find(|t| t.1.name.item == Ident::new("usize"));
                    ensure!(ty.is_some(), "expected to find defined usize");
                    cur = *ty.unwrap().0;
                    item.to.set(cur);
                }
            }
        }
        Ok(cur)
    }

    fn resolve_ref_single(&self, name: Ident) -> Result<TypeId> {
        Ok(match &self.kind {
            mir::TypeKind::Container { fields, .. } => {
                fields
                    .iter()
                    .find(|f| f.name == name)
                    .ok_or_else(|| anyhow!("field not found: {:?}", name))?
                    .ty
                    .target
            }
            _ => bail!("unknown field: {:?}", name),
        })
    }

    fn propogate_types(&mut self, i: TypeId, collector: &TypeUsageCollector) -> Result<()> {
        for (name, param) in self.params.iter_mut() {
            *param = collector.find_type(i, name)?;
        }
        Ok(())
    }

    fn assert_no_unresolved(&self) {
        for (_, par) in self.params.iter() {
            assert!(par.is_valid());
        }
    }
}

pub fn resolve_types(types: &mut HashMap<TypeId, Definition>) -> Result<()> {
    let mut collector = TypeUsageCollector::default();

    loop {
        collector.reset_validity();
        for (i, d) in types.iter() {
            d.resolve_types(*i, &types, &mut collector)?;
        }
        if collector.has_invalid && !collector.has_valid {
            // TODO: print unresolved
            bail!("type resolution stuck");
        } else if !collector.has_invalid && !collector.has_valid {
            break;
        }
        let count_resolved = types.len();
        for (i, d) in types.iter_mut() {
            d.propogate_types(*i, &collector)?;
        }
    }

    for (_, d) in types.iter() {
        d.assert_no_unresolved()
    }

    Ok(())
}
