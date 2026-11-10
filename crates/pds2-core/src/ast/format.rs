use std::rc::Rc;

use dprint_core::formatting::{
	condition_helpers, Condition, ConditionProperties, ConditionResolverContext, Info, LineNumber,
	PrintItems, PrintOptions, Signal,
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
					out.push_str_runtime_width_computed("\"");
					out.push_string(string.to_string());
					out.push_str_runtime_width_computed("\"");
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
				let mut s = String::from("b\"");
				for c in b {
					match c {
						b'"' | b'\\' => s.extend(['\\', *c as char]),
						0x20..=0x7e => s.push(*c as char),
						c => s.push_str(&format!("\\x{c:02x}")),
					}
				}
				s.push('"');
				out.push_string(s);
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

fn print_list<'v>(
	out: &mut PrintItems,
	open: &'static str,
	close: &'static str,
	entries: impl Iterator<Item = (Option<&'v Ident>, &'v Value)>,
) {
	let start = LineNumber::new("args start");
	let end = LineNumber::new("args end");
	out.push_str_runtime_width_computed(open);
	out.push_signal(Signal::StartNewLineGroup);
	out.push_info(start);
	let condition = Rc::new(move |ctx: &mut ConditionResolverContext| {
		condition_helpers::is_multiple_lines(ctx, start, end)
	});
	out.push_condition(Condition::new(
		"args_start_multiline",
		ConditionProperties {
			condition: condition.clone(),
			true_path: {
				let mut items = PrintItems::new();
				items.push_signal(Signal::NewLine);
				items.push_signal(Signal::StartIndent);
				Some(items)
			},
			false_path: None,
		},
	));

	let arg_nl_condition = Condition::new(
		"args_split",
		ConditionProperties {
			condition: condition.clone(),
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
	);
	let mut first = Some(());
	for (k, v) in entries {
		if first.take().is_none() {
			out.push_str_runtime_width_computed(",");
			out.push_condition(arg_nl_condition.clone());
		}
		if let Some(k) = k {
			out.push_string(k.0.to_string());
			out.push_str_runtime_width_computed(": ");
		}
		v.print(out, None);
	}
	out.push_condition(Condition::new(
		"args_end_multiline",
		ConditionProperties {
			condition: condition.clone(),
			true_path: {
				let mut items = PrintItems::new();
				items.push_signal(Signal::FinishIndent);
				items.push_signal(Signal::NewLine);
				Some(items)
			},
			false_path: None,
		},
	));

	out.push_info(end);
	out.push_signal(Signal::FinishNewLineGroup);
	out.push_str_runtime_width_computed(close);
}

impl ItemArgs {
	fn print(&self, out: &mut PrintItems) {
		if !self.is_empty() {
			print_list(
				out,
				"(",
				")",
				self.unnamed
					.iter()
					.map(|v| (None, v))
					.chain(self.named.iter().map(|(k, v)| (Some(k), v))),
			);
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
			(&self.block).print(out);
		}
	}
}

pub fn format(input: &Block) -> String {
	dprint_core::formatting::format(
		|| {
			let mut out = PrintItems::new();
			input.print(&mut out);
			out
		},
		PrintOptions {
			max_width: 100,
			indent_width: 3,
			use_tabs: true,
			new_line_text: "\n",
		},
	)
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
		] {
			let out = super::format(&crate::ast::parse(src).unwrap());
			assert_eq!(squash(&out), squash(src));
		}
	}

	#[test]
	fn data_literals_roundtrip_through_the_formatter() {
		for src in [
			"def(a) => x(v: (name: \"n\", weight: -1.5, big: 1e300, tags: [a, (b: 2)], empty: ()));",
			"def(a) => x(v: T(raw: b\"\\x12\\x02q\\\"\\\\\", xs: []));",
		] {
			let out = super::format(&crate::ast::parse(src).unwrap());
			assert_eq!(squash(&out), squash(src));
		}
		assert!(crate::ast::parse("def(a) => x(v: (k: 1, k: 2));").is_err());
		assert!(crate::ast::parse("def(a) => x(k: 1, k: 2);").is_err());
	}
}
