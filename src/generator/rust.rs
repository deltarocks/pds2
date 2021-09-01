use crate::ast::Ident;
use crate::hir::TypeId;
use crate::mir::{self, Field, Type, TypeKind, TypeMap};
use crate::pi;

use super::GeneratorOutput;
use anyhow::Result;
use dprint_core::formatting::{PrintItems, Signal};

impl TypeMap {
    fn name(&self, type_id: TypeId) -> String {
        assert!(type_id.is_valid());
        format!("pds_{}", self.0[&type_id].name.readable_name())
    }
    fn ty(&self, type_id: TypeId) -> String {
        format!("super::{}::Ty", self.name(type_id))
    }
    fn virt(&self, type_id: TypeId, field: Ident) -> String {
        format!("super::{}::virtual_{:?}", self.name(type_id), field)
    }
    fn args(&self, type_id: TypeId) -> String {
        format!("super::{}::Args", self.name(type_id))
    }
    fn serialize(&self, type_id: TypeId) -> String {
        format!("super::{}::serialize", self.name(type_id))
    }
    fn deserialize(&self, type_id: TypeId) -> String {
        format!("super::{}::deserialize", self.name(type_id))
    }
    fn size_of(&self, type_id: TypeId) -> String {
        format!("super::{}::size_of", self.name(type_id))
    }
}

pub fn generate_read_reference(types: &TypeMap, from_locals: bool, input: &Field) -> PrintItems {
    let mut pi = PrintItems::new();
    pi!(pi => "&{" INDENT NL);
    for (i, field) in input.0.iter().enumerate() {
        match field.kind {
            mir::FieldItemKind::Argument => {
                pi!(pi =>
                    "let _0 = &args;" NL
                    "let _0: &" {=types.ty(field.to.get())} " = &_0." {?field.name} ";" NL
                );
            }
            mir::FieldItemKind::Field => {
                if i == 0 && from_locals {
                    pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &" {?field.name} ";" NL)
                } else {
                    pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &_0." {?field.name} ";" NL)
                }
            }
            mir::FieldItemKind::Virtual => {
                pi!(pi => "let _0: &" {=types.ty(field.to.get())} " = &" {=types.virt(field.from.get(), field.name.clone())} "(&_0);" NL);
            }
        }
    }
    pi!(pi => "(*_0).try_into().unwrap()" NL);
    pi!(pi => DEDENT "}");
    pi
}

pub fn generate_args_passing(types: &mir::TypeMap, input: &Type) -> PrintItems {
    let mut pi = PrintItems::new();
    pi!(pi => {=types.args(input.target)} " {" INDENT NL);
    for (name, field) in input.args.iter() {
        pi!(pi => {?name} ": " {<generate_read_reference(types, true, &field)} "," NL);
    }
    pi!(pi => DEDENT "}");
    pi
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput) -> Result<()> {
    let mut pi = PrintItems::new();
    pi!(pi => "#![allow(non_camel_case_types, dead_code, unused_imports)]" NL);
    pi!(pi => "use crate::pds::{Input, Output, Result};" NL);
    for (id, ty) in &input.0 {
        let name = ty.name.readable_name();
        if !ty.kind.is_native() {
            pi!(pi => "/// " {?ty.name} NL);
            pi!(pi => "mod " {=input.name(*id)} " {" INDENT NL);
            pi!(pi => "use std::convert::TryInto;" NL);
            pi!(pi => "use super::{Input, Output, Result};" NL);

            let has_args = !ty.params.is_empty();
            if has_args {
                pi!(pi => "pub struct Args<'a> {" INDENT NL);
                for (name, par) in ty.params.iter() {
                    pi!(pi => "pub " {?name} ": &'a " {=input.ty(*par)} "," NL);
                }
                pi!(pi => DEDENT "}" NL);
            }
            match &ty.kind {
                TypeKind::Container { fields, .. } => {
                    pi!(pi =>
                        "pub struct Ty {" INDENT NL
                            {forin field in fields.iter() =>
                                {ifelse field.load_from.is_none() =>
                                    {"pub " {?field.name} ": " {=input.ty(field.ty.target)} "," NL}
                                    {"// " {?field.name} " - virtual field" NL}
                                }
                            }
                        DEDENT "}" NL

                        "pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {" INDENT NL
                            {forin field in fields.iter() =>
                                {ifelse field.load_from.is_none() =>
                                    {{=input.serialize(field.ty.target)}"(&_0." {?field.name} ", output)?;" NL}
                                    {{=input.serialize(field.ty.target)}"(" INDENT NL
                                        {<generate_read_reference(input, false, field.load_from.as_ref().unwrap())} "," NL
                                        "output," NL
                                    DEDENT ")?;" NL}
                                }
                            }
                            "Ok(())" NL
                        DEDENT "}" NL

                        "pub fn deserialize(input: &mut Input"
                        {if has_args => " , args: Args<'_>"}
                        ") -> Result<Ty> {" INDENT NL
                            {forin field in fields.iter() =>
                                "let " {?field.name} " = " {=input.deserialize(field.ty.target)} "(input"
                                {if !input[field.ty.target].params.is_empty() => ", " {<generate_args_passing(input, &field.ty)}}
                                ")?;" NL
                            }

                            "Ok(Ty {" INDENT NL
                                {forin field in fields.iter().filter(|f| f.load_from.is_none()) =>
                                    {?field.name} "," NL
                                }
                            DEDENT "})" NL
                        DEDENT "}" NL

                        "pub fn size_of(_0: &Ty) -> usize {" INDENT NL
                            {forin i in fields.iter().enumerate() =>
                                {if i.0 != 0 => " + "}
                                {=input.size_of(i.1.ty.target)} "("
                                {ifelse i.1.load_from.is_none() =>
                                    {"&_0." {?i.1.name}}
                                    {{<generate_read_reference(input, false, i.1.load_from.as_ref().unwrap())}}
                                }
                                ")"
                            }
                            NL
                        DEDENT "}" NL
                    );
                }
                TypeKind::DynamicArray { size_ref, t } => {
                    pi!(pi =>
                        "pub type Ty = Vec<" {=input.ty(t.target)} ">;" NL

                        "pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {" INDENT NL
                            "for item in _0 {" INDENT NL
                                {=input.serialize(t.target)} "(&item, output)?;" NL
                            DEDENT "}" NL
                            "Ok(())" NL
                        DEDENT "}" NL

                        "pub fn deserialize(input: &mut Input"
                        {if has_args => " , args: Args<'_>"}
                        ") -> Result<Ty> {" INDENT NL
                            "let mut out = Vec::with_capacity(*args." {?size_ref.0[0].1} " as usize);" NL
                            "for _ in 0..(*args." {?size_ref.0[0].1} " as usize) {" INDENT NL
                                "out.push(" {=input.deserialize(t.target)} "(input)?);" NL
                            DEDENT "}" NL
                            "Ok(out)" NL
                        DEDENT "}" NL

                        "pub fn size_of(_0: &Ty) -> usize {" INDENT NL
                            "_0" NL INDENT
                                ".iter()" NL
                                ".map(|v| " {=input.size_of(t.target)} "(&v))" NL
                                ".sum()" NL DEDENT
                        DEDENT "}" NL

                        "pub fn virtual_len(_0: &Ty) -> super::pds_usize::Ty {" INDENT NL
                            "_0.len() as super::pds_usize::Ty" NL
                        DEDENT "}" NL
                    );
                }
                TypeKind::Union {
                    tag,
                    matches,
                    default,
                } => {
                    pi!(pi =>
                        "pub enum Ty {" INDENT NL
                            {forin m in matches =>
                                {?m.1} "(" {=input.ty(m.2.target)} ")," NL
                            }
                            {if default.is_some() =>
                                {?default.as_ref().unwrap().0} "(" {=input.ty(default.as_ref().unwrap().1.target)} ")," NL
                            }
                        DEDENT "}" NL
                        "pub fn serialize(_0: &Ty, output: &mut Output) -> Result<()> {" INDENT NL
                            "todo!()" NL
                        DEDENT "}" NL
                        "pub fn deserialize(input: &mut Input"
                        {if has_args => " , args: Args<'_>"}
                        ") -> Result<Ty> {" INDENT NL
                            "todo!()" NL
                        DEDENT "}" NL

                        "pub fn size_of(_0: &Ty) -> usize {" INDENT NL
                            "match &_0 {" INDENT NL
                                {forin m in matches =>
                                    "Self::" {?m.1} "(_0) => " {=input.size_of(m.2.target)} "(_0)," NL
                                }
                                {if default.is_some() =>
                                    "Self::" {?default.as_ref().unwrap().0} "(_0) => " {=input.size_of(default.as_ref().unwrap().1.target)} "(_0)," NL
                                }
                            DEDENT "}" NL
                        DEDENT "}" NL
                        "pub fn virtual_tag(_0: &Ty) -> super::pds_usize::Ty {" INDENT NL
                            "match &_0 {" INDENT NL
                                {forin m in matches =>
                                    "Self::" {?m.1} "(_0) => " {?m.0} "," NL
                                }
                                {if default.is_some() =>
                                    "Self::" {?default.as_ref().unwrap().0} "(_0) => " {=input.size_of(default.as_ref().unwrap().1.target)} "(_0)," NL
                                }
                            DEDENT "}" NL
                        DEDENT "}" NL
                    );
                }
                TypeKind::FixedSizeArray { size, t } => {
                    pi!(pi =>
                        "type " {=name} " = ["{=input[t.target].name.readable_name()}"; "{=size}"];"

                    );
                }
                TypeKind::Native(_) => unreachable!(),
            }
            pi!(pi => DEDENT "}" NL);
        } else {
            pi!(pi =>
                "use crate::pds::pds_" {=ty.name.readable_name()}  ";" NL
            );
        }
    }
    output.insert("mod.rs".into(), pi);
    Ok(())
}
