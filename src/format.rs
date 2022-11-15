use dprint_core::formatting::PrintItems;
use jrsonnet_evaluator::{throw, typed::Typed, ObjValue, Result, Val};

use crate::pi;

enum FormatItem {
	Nl,
	Indent,
	Dedent,
	Str(String),
}

pub fn convert_format(input: &Val) -> Result<PrintItems> {
	let mut out = Vec::new();
	convert_format_into(input, &mut out)?;
	let mut fmt = PrintItems::new();
	for item in out {
		match item {
			FormatItem::Nl => pi!(fmt => NL),
			FormatItem::Indent => pi!(fmt => INDENT),
			FormatItem::Dedent => pi!(fmt => DEDENT),
			FormatItem::Str(s) => pi!(fmt => {=s}),
		}
	}
	Ok(fmt)
}
fn convert_format_into(input: &Val, out: &mut Vec<FormatItem>) -> Result<()> {
	match input {
		Val::Null => {}
		Val::Str(s) => match s.as_str() {
			"NL" => out.push(FormatItem::Nl),
			"INDENT" => out.push(FormatItem::Indent),
			"DEDENT" => out.push(FormatItem::Dedent),
			_ => throw!("unexpected string: {}", s),
		},
		Val::Arr(a) => {
			for i in a.iter() {
				let i = i?;
				convert_format_into(&i, out)?;
			}
		}
		Val::Obj(o) => {
			let fields = o.fields();
			if fields.len() != 1 {
				throw!("unexpected object")
			}
			let field = fields[0].clone();
			if &field != "Str" {
				throw!("unexpected object");
			}
			let value = o.get(field)?.expect("field exists");
			let Some(str) = value.as_str() else {
				throw!("unexpected object");
			};
			out.push(FormatItem::Str(str.to_string()))
		}
		_ => throw!("unexpected input value: {}", input.to_string()?),
	}
	Ok(())
}

#[derive(Typed)]
pub struct SpecializerObj {
	pub specializer: ObjValue,
}

impl Default for SpecializerObj {
	fn default() -> Self {
		Self {
			specializer: ObjValue::new_empty(),
		}
	}
}
