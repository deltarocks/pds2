use crate::ast::Ident;
use crate::hir::TypeId;
use crate::mir::{self, Field, Type, TypeKind, TypeMap};
use crate::pi;

use super::GeneratorOutput;
use anyhow::Result;
use dprint_core::formatting::{PrintItems, Signal};

impl TypeMap {
    fn name(&self, type_id: TypeId) -> String {
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
}

pub fn generate_write_reference(input: &Field) -> PrintItems {
    let mut pi = PrintItems::new();
    pi!(pi => "value");
    for field in input.0.iter() {
        match field {
            mir::FieldItem::Argument { name, value } => {
                unreachable!()
            }
            mir::FieldItem::Field { ty, name, value } => {
                pi!(pi => "/*" {?ty} "*/" "." {?name})
            }
            mir::FieldItem::Virtual { ty, name } => {
                pi!(pi => "/*" {?ty} "*/" "." {?name} "()")
            }
        }
    }
    pi
}
pub fn generate_read_reference(types: &TypeMap, input: &Field) -> PrintItems {
    let mut pi = PrintItems::new();
    pi!(pi => "{" INDENT NL);
    for (i, field) in input.0.iter().enumerate() {
        match field.kind {
            mir::FieldItemKind::Argument => {
                pi!(pi =>
                    "let _0 = &args;" NL
                    "let _0: " {=types.ty(field.to.get())} " = &_0." {?field.name} ";" NL
                );
            }
            mir::FieldItemKind::Field => {
                pi!(pi => "let _0: " {=types.ty(field.to.get())} " = &_0." {?field.name} ";" NL);
            }
            mir::FieldItemKind::Virtual => {
                pi!(pi => "let _0: " {=types.ty(field.to.get())} " = &" {=types.virt(field.from.get(), field.name.clone())} "(&_0);" NL);
            }
        }
    }
    pi!(pi => DEDENT "}");
    pi
}

pub fn generate_args_passing(types: &mir::TypeMap, input: &Type) -> PrintItems {
    let mut pi = PrintItems::new();
    pi!(pi => {=types.args(input.target)} " {" INDENT NL);
    for (name, field) in input.args.iter() {
        pi!(pi => {?name} ": &" {<generate_read_reference(types, &field)} "," NL);
    }
    pi!(pi => DEDENT "}");
    pi
}

pub fn generator(input: &mir::TypeMap, output: &mut GeneratorOutput) -> Result<()> {
    let mut pi = PrintItems::new();
    pi!(pi => "#![allow(non_camel_case_types, dead_code, unused_imports)]" NL);
    pi!(pi => "use crate::pds::{Input, Output, Result};" NL);
    for (id, ty) in input.0 {
        let name = ty.name.readable_name();
        if !ty.kind.is_native() {
            pi!(pi => "/// " {?ty.name} NL);
            pi!(pi => "mod " {=input.name(id)} " {" INDENT NL);

            let has_args = !ty.params.is_empty();
            if has_args {
                pi!(pi => "pub struct Args<'a> {" INDENT NL);
                for (name, par) in ty.params.iter() {
                    pi!(pi => {?name} ": &'a super::" {=input.name(*par)} "," NL);
                }
                pi!(pi => DEDENT "}" NL);
            }
            match &ty.kind {
                TypeKind::Container { fields, .. } => {
                    pi!(pi =>
                        "struct Ty {" INDENT NL
                            {forin field in fields.iter() =>
                                {ifelse field.load_from.is_none() =>
                                    {{?field.name} ": " {=input.ty(field.ty.target)} "," NL}
                                    {"// " {?field.name} " - virtual field" NL}
                                }
                            }
                        DEDENT "}" NL

                        "fn serialize(value: &Ty, output: &mut Output) -> Result<()> {" INDENT NL
                            {forin field in fields.iter() =>
                                {ifelse field.load_from.is_none() =>
                                    {{=input.serialize(field.ty.target)}"(value." {?field.name} ", output)?;" NL}
                                    {{=input.serialize(field.ty.target)}"(" INDENT NL
                                        "output," NL
                                        {<generate_write_reference(field.load_from.as_ref().unwrap())} "," NL
                                    DEDENT ")?;" NL}
                                }
                            }
                            "Ok(())" NL
                        DEDENT "}" NL

                        "fn deserialize(input: &mut Input"
                        {if has_args => "," NL "args: Args<'_>," NL}
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

                        "fn size_of(value: &Ty) -> usize {" INDENT NL
                            {forin i in fields.iter().enumerate() =>
                                {if i.0 != 0 => "+"}
                                {=input[i.1.ty.target].name.readable_name()} "_size_of("
                                {ifelse i.1.load_from.is_none() =>
                                    {"&value." {?i.1.name}}
                                    {"&" {<generate_write_reference(i.1.load_from.as_ref().unwrap())}}
                                }
                                ")"
                            }
                        DEDENT "}" NL
                    );
                }
                TypeKind::DynamicArray { size_ref, t } => {
                    let el_name = input[&t.target].name.readable_name();
                    pi!(pi =>
                        "type Ty = Vec<" {=el_name} ">;" NL

                        "fn " {=name} "_serialize(output: &mut Output, value: " {=name} ") -> Result<()> {" INDENT NL
                            "for item in value {" INDENT NL
                                {=el_name} "_serialize(output, item)?;" NL
                            DEDENT "}" NL
                            "Ok(())" NL
                        DEDENT "}" NL

                        "fn " {=name} "_deserialize" "(" INDENT NL "input: &mut Input"
                        {if has_args => "," NL "args: " {=name} "_args<'_>," NL}
                        DEDENT ") -> Result<" {=name} "> {" INDENT NL
                            "let mut out = Vec::with_capacity(*args." {?size_ref.0[0].1} " as usize);" NL
                            "for _ in 0..(*args." {?size_ref.0[0].1} " as usize) {" INDENT NL
                                "out.push(" {=el_name} "_deserialize(input)?);" NL
                            DEDENT "}" NL
                            "Ok(out)" NL
                        DEDENT "}" NL

                        "fn " {=name} "_size_of(value: &" {=name} ") -> usize {" INDENT NL
                            "value" NL INDENT
                                ".iter()" NL
                                ".map(|v| " {=el_name} "_size_of(&v))" NL
                                ".sum()" NL DEDENT
                        DEDENT "}" NL
                    );
                }
                TypeKind::Union { matches, default } => todo!(),
                TypeKind::FixedSizeArray { size, t } => {
                    pi!(pi =>
                        "type " {=name} " = ["{=input[&t.target].name.readable_name()}"; "{=size}"];"

                    );
                }
                TypeKind::Native(_) => unreachable!(),
            }
            pi!(pi => DEDENT "}" NL);
        } else {
            pi!(pi => 
            "use crate::pds::{" INDENT NL
                {=ty.name.readable_name()} "," NL
                {=ty.name.readable_name()} "_serialize" "," NL
                {=ty.name.readable_name()} "_deserialize" "," NL
                {=ty.name.readable_name()} "_size_of" "," NL
            DEDENT "};" NL);
        }
    }
    output.insert("mod.rs".into(), pi);
    Ok(())
}
