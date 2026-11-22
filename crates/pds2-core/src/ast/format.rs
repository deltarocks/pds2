use std::rc::Rc;

use std::cell::Cell;

use dprint_core::formatting::condition_helpers;
use dprint_core::formatting::conditions::{if_true, if_true_or};
use dprint_core::formatting::ir_helpers::with_indent;
use dprint_core::formatting::{
	Condition, ConditionProperties, ConditionReference, ConditionResolver,
	ConditionResolverContext, Info, LineNumber, PrintItems, PrintOptions, Signal,
};

use super::{Block, Ident, Item, ItemArgs, Statement, Value};

impl Block {
	fn print(&self, out: &mut PrintItems) {
		if !self.root {
			out.push_str_runtime_width_computed("{");

			out.push_signal(Signal::NewLine);
			out.push_signal(Signal::StartIndent);
			out.push_signal(Signal::StartNewLineGroup);
		}

		let docs = self.attributes.iter().filter(|(k, _)| k.as_str() == "doc");
		let rest = self.attributes.iter().filter(|(k, _)| k.as_str() != "doc");
		for (k, v) in docs.chain(rest) {
			out.push_str_runtime_width_computed("@!");
			out.push_string(k.to_string());
			out.push_str_runtime_width_computed(" ");
			for (i, item) in v.iter().enumerate() {
				if i != 0 {
					out.push_str_runtime_width_computed(" => ");
				}
				item.print(out, None);
			}
			out.push_signal(Signal::NewLine);
		}
		if !self.attributes.is_empty() && !self.statements.is_empty() {
			out.push_signal(Signal::NewLine);
		}

		for (i, stmt) in self.statements.iter().enumerate() {
			if i != 0 {
				out.push_signal(Signal::NewLine);
			}
			out.push_signal(Signal::StartNewLineGroup);
			stmt.print(out);
			out.push_str_runtime_width_computed(";");
			out.push_signal(Signal::NewLine);
			out.push_signal(Signal::FinishNewLineGroup);
		}

		if !self.root {
			out.push_signal(Signal::FinishNewLineGroup);
			out.push_signal(Signal::FinishIndent);
			out.push_str_runtime_width_computed("}");
		}
	}
}

impl Statement {
	fn print(&self, out: &mut PrintItems) {
		for (k, v) in self.attributes.iter().filter(|k| k.0 == &Ident::new("doc")) {
			out.push_str_runtime_width_computed("@");
			out.push_string(k.0.to_string());
			out.push_str_runtime_width_computed(" ");
			for item in v.iter() {
				item.print(out, None);
			}
			out.push_signal(Signal::NewLine);
		}
		let start = LineNumber::new("start");
		let end = LineNumber::new("end");
		out.push_info(start);
		for (k, v) in self.attributes.iter().filter(|k| k.0 != &Ident::new("doc")) {
			out.push_str_runtime_width_computed("@");
			out.push_string(k.0.to_string());
			out.push_str_runtime_width_computed(" ");
			for item in v.iter() {
				item.print(out, None);
			}
			out.push_condition(Condition::new(
				"attributes_multiline",
				ConditionProperties {
					condition: Rc::new(move |ctx| {
						condition_helpers::is_multiple_lines(ctx, start, end)
					}),
					true_path: {
						let mut items = PrintItems::new();
						items.push_signal(Signal::NewLine);
						Some(items)
					},
					false_path: {
						let mut items = PrintItems::new();
						items.push_signal(Signal::SpaceOrNewLine);
						Some(items)
					},
				},
			));
		}

		out.push_signal(Signal::StartNewLineGroup);
		{
			let start_items = LineNumber::new("start_items");
			let end_items = LineNumber::new("end_items");
			out.push_info(start_items);
			let last_idx = self.items.len() - 1;
			for (i, item) in self.items.iter().enumerate() {
				if i != 0 {
					out.push_condition(Condition::new(
						"items multiline",
						ConditionProperties {
							condition: Rc::new(move |ctx| {
								condition_helpers::is_multiple_lines(ctx, start_items, end_items)
							}),
							true_path: {
								let mut items = PrintItems::new();
								items.push_signal(Signal::NewLine);
								Some(items)
							},
							false_path: {
								let mut items = PrintItems::new();
								items.push_signal(Signal::SpaceOrNewLine);
								Some(items)
							},
						},
					));
					out.push_str_runtime_width_computed("=> ");
				}
				item.print(out, (last_idx == i).then(|| end_items.into()));
			}
		}
		out.push_signal(Signal::FinishNewLineGroup);
		out.push_info(end);
	}
}

impl Value {
	fn print(&self, out: &mut PrintItems, before_block: Option<Info>) {
		match self {
			Value::String { string, is_block } => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				if !is_block {
					out.push_string(string_literal(string));
				} else {
					out.push_str_runtime_width_computed("\"\"\"");
					for line in string.lines() {
						out.push_signal(Signal::NewLine);
						out.push_string(line.trim().to_string());
					}
					out.push_signal(Signal::NewLine);
					out.push_str_runtime_width_computed("\"\"\"");
				}
			}
			Value::FieldRef(r) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				out.push_string(format!("{:?}", r));
			}
			Value::Int(v) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				out.push_string(format!("{:?}", v))
			}
			Value::Float(v) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				out.push_string(format!("{:?}", v.0))
			}
			Value::Bytes(b) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				out.push_string(bytes_literal(b));
			}
			Value::List(items) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				print_list(out, "[", "]", items.iter().map(|v| (None, v)));
			}
			Value::Record(fields) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				print_list(out, "(", ")", fields.iter().map(|(k, v)| (Some(k), v)));
			}
			Value::Item(i) => {
				i.print(out, before_block);
			}
			Value::Binary(op, a, b) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				let operand = |out: &mut PrintItems, v: &Value, right: bool| {
					let paren = matches!(v, Value::Binary(inner, ..) if inner.precedence() < op.precedence() || (right && inner.precedence() == op.precedence()));
					if paren {
						out.push_str_runtime_width_computed("(");
					}
					v.print(out, None);
					if paren {
						out.push_str_runtime_width_computed(")");
					}
				};
				operand(out, a, false);
				out.push_str_runtime_width_computed(" ");
				out.push_str_runtime_width_computed(op.symbol());
				out.push_str_runtime_width_computed(" ");
				operand(out, b, true);
			}
			Value::Internal(i) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				out.push_str_runtime_width_computed("<! ");
				out.push_string(i.to_string());
				out.push_str_runtime_width_computed(" !>");
			}
			Value::Unset => unreachable!(),
			Value::FunctionCall(n, args) => {
				out.push_str_runtime_width_computed("%");
				out.push_string(n.as_str().to_string());
				args.print(out)
			}
		}
	}
}

fn string_literal(string: &str) -> String {
	let mut s = String::from("\"");
	for c in string.chars() {
		match c {
			'"' | '\\' => s.extend(['\\', c]),
			'\n' => s.push_str("\\n"),
			'\r' => s.push_str("\\r"),
			'\t' => s.push_str("\\t"),
			'\0' => s.push_str("\\0"),
			c if c.is_control() => s.push_str(&format!("\\u{{{:x}}}", c as u32)),
			c => s.push(c),
		}
	}
	s.push('"');
	s
}

fn bytes_literal(bytes: &[u8]) -> String {
	let mut s = String::from("b\"");
	for c in bytes {
		match c {
			b'"' | b'\\' => s.extend(['\\', *c as char]),
			0x20..=0x7e => s.push(*c as char),
			c => s.push_str(&format!("\\x{c:02x}")),
		}
	}
	s.push('"');
	s
}

fn print_list<'v>(
	out: &mut PrintItems,
	open: &'static str,
	close: &'static str,
	entries: impl Iterator<Item = (Option<&'v Ident>, &'v Value)>,
) {
	let entries: Vec<_> = entries.collect();
	let width = entries_width(&entries, MAX_WIDTH).map(|w| (w + close.len() + 1) as u32);
	let fits: ConditionResolver = Rc::new(move |ctx: &mut ConditionResolverContext| {
		Some(width.is_none_or(|w| ctx.writer_info.column_number + w > MAX_WIDTH as u32))
	});
	let body_ref = Rc::new(Cell::new(None::<ConditionReference>));
	let multi_line: ConditionResolver = {
		let body_ref = body_ref.clone();
		Rc::new(move |ctx: &mut ConditionResolverContext| ctx.resolved_condition(&body_ref.get()?))
	};

	let mut items = PrintItems::new();
	let mut entries = entries.into_iter().peekable();
	while let Some((k, v)) = entries.next() {
		if let Some(k) = k {
			items.push_string(k.0.to_string());
			items.push_str_runtime_width_computed(": ");
		}
		v.print(&mut items, None);
		if entries.peek().is_some() {
			items.push_str_runtime_width_computed(",");
			items.push_condition(if_true_or(
				"list separator",
				multi_line.clone(),
				Signal::NewLine.into(),
				" ".into(),
			));
		} else {
			items.push_condition(if_true("trailing comma", multi_line.clone(), ",".into()));
		}
	}
	let items = items.into_rc_path();
	let mut indented = PrintItems::new();
	indented.push_signal(Signal::NewLine);
	indented.extend(items.into());
	let mut indented = with_indent(indented);
	indented.push_signal(Signal::NewLine);

	let mut body = if_true_or("list body", fits, indented, items.into());
	body_ref.set(Some(body.create_reference()));
	out.push_str_runtime_width_computed(open);
	out.push_condition(body);
	out.push_str_runtime_width_computed(close);
}

const MAX_WIDTH: usize = 100;

fn entries_width(entries: &[(Option<&Ident>, &Value)], budget: usize) -> Option<usize> {
	let mut total = 2 * entries.len().saturating_sub(1);
	for (k, v) in entries {
		total += k.map_or(0, |k| k.as_str().len() + 2);
		total += value_width(v, budget.checked_sub(total)?)?;
	}
	(total <= budget).then_some(total)
}

fn list_width(
	open_close: usize,
	entries: &[(Option<&Ident>, &Value)],
	budget: usize,
) -> Option<usize> {
	Some(open_close + entries_width(entries, budget.checked_sub(open_close)?)?)
}

fn value_width(v: &Value, budget: usize) -> Option<usize> {
	let text = |s: String| Some(s.chars().count());
	let width = match v {
		Value::String {
			string,
			is_block: false,
		} => text(string_literal(string))?,
		Value::String { is_block: true, .. } | Value::Unset => return None,
		Value::FieldRef(r) => text(format!("{r:?}"))?,
		Value::Int(n) => text(format!("{n:?}"))?,
		Value::Float(f) => text(format!("{:?}", f.0))?,
		Value::Bytes(b) => text(bytes_literal(b))?,
		Value::Internal(i) => i.chars().count() + 6,
		Value::List(items) => {
			let entries: Vec<_> = items.iter().map(|v| (None, v)).collect();
			list_width(2, &entries, budget)?
		}
		Value::Record(fields) => {
			let entries: Vec<_> = fields.iter().map(|(k, v)| (Some(k), v)).collect();
			list_width(2, &entries, budget)?
		}
		Value::Item(i) => {
			if !i.block.statements.is_empty() {
				return None;
			}
			let name = format!("{:?}", i.name).chars().count();
			name + args_width(&i.args, budget.checked_sub(name)?)?
		}
		Value::FunctionCall(n, args) => {
			let name = n.as_str().len() + 1;
			name + args_width(args, budget.checked_sub(name)?)?
		}
		Value::Binary(op, a, b) => {
			let paren = |v: &Value, right: bool| match v {
				Value::Binary(inner, ..)
					if inner.precedence() < op.precedence()
						|| (right && inner.precedence() == op.precedence()) =>
				{
					2
				}
				_ => 0,
			};
			let a = value_width(a, budget)? + paren(a, false);
			let b = value_width(b, budget)? + paren(b, true);
			a + b + op.symbol().len() + 2
		}
	};
	(width <= budget).then_some(width)
}

fn args_width(args: &ItemArgs, budget: usize) -> Option<usize> {
	if args.is_empty() {
		return Some(0);
	}
	let entries: Vec<_> = args.ordered().collect();
	list_width(2, &entries, budget)
}

impl ItemArgs {
	fn print(&self, out: &mut PrintItems) {
		if !self.is_empty() {
			print_list(out, "(", ")", self.ordered());
		}
	}
}

impl Item {
	fn print(&self, out: &mut PrintItems, before_block: Option<Info>) {
		out.push_string(format!("{:?}", self.name));
		self.args.print(out);
		if let Some(before_block) = before_block {
			out.push_info(before_block);
		}

		if !self.block.statements.is_empty() {
			out.push_str_runtime_width_computed(" ");
			self.block.print(out);
		}
	}
}

fn print_with(print: impl Fn(&mut PrintItems)) -> String {
	dprint_core::formatting::format(
		|| {
			let mut out = PrintItems::new();
			print(&mut out);
			out
		},
		PrintOptions {
			max_width: MAX_WIDTH as u32,
			indent_width: 3,
			use_tabs: true,
			new_line_text: "\n",
		},
	)
}

pub fn format(input: &Block) -> String {
	print_with(|out| input.print(out))
}

pub fn format_value(input: &Value) -> String {
	print_with(|out| input.print(out, None))
}

pub fn format_call(head: &str, input: &Value) -> String {
	print_with(|out| {
		out.push_string(head.to_owned());
		match input {
			Value::Record(fields) if !fields.is_empty() => {
				print_list(out, "(", ")", fields.iter().map(|(k, v)| (Some(k), v)))
			}
			v => print_list(out, "(", ")", std::iter::once((None, v))),
		}
	})
}

#[cfg(test)]
mod tests {
	fn squash(s: &str) -> String {
		s.split_whitespace().collect::<Vec<_>>().join(" ")
	}

	#[test]
	fn arithmetic_roundtrips_through_the_formatter() {
		for src in [
			"def(a) => x(n: ../len - 1);",
			"def(a) => x(n: (../a + 1) * 2);",
			"def(a) => x(n: ../a - (../b - 1));",
			"def(a) => x(n: u16(../a) * 2 + -1);",
			"fn(f, b: u8, a, c: u16) => (a + 1) * b;",
		] {
			let out = super::format(&crate::ast::parse(src).unwrap());
			assert_eq!(squash(&out), squash(src));
		}
	}

	fn reformat(src: &str) -> String {
		let out = super::format(&crate::ast::parse(src).unwrap());
		assert_eq!(
			super::format(&crate::ast::parse(&out).unwrap()),
			out,
			"not idempotent"
		);
		out
	}

	#[test]
	fn lists_break_only_when_they_do_not_fit() {
		let out = reformat(
			"def(a) => x(v: (breakers: (thresholds: [(priority: HIGH, max_connections: (value: 1024), max_retries: (value: 3))]), name: \"backend\", other: \"a long enough string to wrap the outer record\"));",
		);
		assert_eq!(
			out,
			"def(a)\n=> x(\n\tv: (\n\t\tbreakers: (\n\t\t\tthresholds: [(priority: HIGH, max_connections: (value: 1024), max_retries: (value: 3))],\n\t\t),\n\t\tname: \"backend\",\n\t\tother: \"a long enough string to wrap the outer record\",\n\t),\n);\n"
		);
	}

	#[test]
	fn nested_lists_keep_indents_balanced() {
		reformat(
			r#"def(a) => x(v: T(file: [(message_type: [(field: [(name: "field_presence", number: 1,
			label: LABEL_OPTIONAL, type: TYPE_ENUM, json_name: "fieldPresence", options: (edition_defaults: [
			(edition: EDITION_LEGACY, value: "EXPLICIT"), (edition: EDITION_PROTO3, value: "IMPLICIT"),
			(edition: EDITION_2023, value: "EXPLICIT")]))])])]));"#,
		);
	}

	#[test]
	fn large_nested_values_format() {
		let item = "(name: \"some fairly long name\", tags: [(key: \"k\", value: [1, 2, 3]), (key: \"other key\", value: [(deep: [\"x\", \"y\"])])])";
		let list = vec![item; 40].join(", ");
		let out = reformat(&format!("def(a) => x(v: [{list}, [[{list}]]]);"));
		assert!(
			out.lines().all(|l| l.replace('\t', "   ").len() <= 100),
			"{out}"
		);
	}

	#[test]
	fn data_literals_roundtrip_through_the_formatter() {
		for src in [
			"def(a) => x(v: (name: \"n\", weight: -1.5, big: 1e300, tags: [a, (b: 2)], empty: ()));",
			"def(a) => x(v: T(raw: b\"\\x12\\x02q\\\"\\\\\", xs: []));",
			"def(a) => x(v: \"q\\\"\\\\\\n\\t\\0\\u{1b}\");",
		] {
			let out = super::format(&crate::ast::parse(src).unwrap());
			assert_eq!(squash(&out), squash(src));
		}
		assert!(matches!(
			crate::ast::parse_expr("\"\\u{e9}\""),
			Ok(crate::ast::Value::String { string, .. }) if string == "\u{e9}"
		));
		assert!(crate::ast::parse_expr("\"\\q\"").is_err());
		assert!(crate::ast::parse("def(a) => x(v: (k: 1, k: 2));").is_err());
		assert!(crate::ast::parse("def(a) => x(k: 1, k: 2);").is_err());
	}
}
