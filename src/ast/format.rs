use std::rc::Rc;

use dprint_core::formatting::*;

use super::{Block, Ident, Item, ItemArgs, Statement, Value};

impl Block {
	fn print(&self, out: &mut PrintItems) {
		if !self.root {
			out.push_str("{");
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
			out.push_str(";");
			out.push_signal(Signal::NewLine);
			out.push_signal(Signal::FinishNewLineGroup);
		}

		if !self.root {
			out.push_signal(Signal::FinishNewLineGroup);
			out.push_signal(Signal::FinishIndent);
			out.push_str("}");
		}
	}
}

impl Statement {
	fn print(&self, out: &mut PrintItems) {
		for (k, v) in self.attributes.iter().filter(|k| k.0 == &Ident::new("doc")) {
			out.push_str("@");
			out.push_string(k.0.to_string());
			out.push_str(" ");
			for item in v.iter() {
				item.print(out, None);
			}
			out.push_signal(Signal::NewLine);
		}
		let start = LineNumber::new("start");
		let end = LineNumber::new("end");
		out.push_info(start);
		for (k, v) in self.attributes.iter().filter(|k| k.0 != &Ident::new("doc")) {
			out.push_str("@");
			out.push_string(k.0.to_string());
			out.push_str(" ");
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
					out.push_str("=> ");
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
					out.push_str("\"");
					out.push_string(string.to_string());
					out.push_str("\"");
				} else {
					out.push_str("\"\"\"");
					for line in string.lines() {
						out.push_signal(Signal::NewLine);
						out.push_string(line.trim().to_string());
					}
					out.push_signal(Signal::NewLine);
					out.push_str("\"\"\"");
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
			Value::Item(i) => {
				i.print(out, before_block);
			}
			Value::Internal(i) => {
				if let Some(before_block) = before_block {
					out.push_info(before_block);
				}
				out.push_str("<! ");
				out.push_string(i.to_string());
				out.push_str(" !>");
			}
			Value::Unset => unreachable!(),
			Value::FunctionCall(n, args) => {
				out.push_str("%");
				out.push_string(n.as_str().to_string());
				args.print(out)
			}
		}
	}
}

impl ItemArgs {
	fn print(&self, out: &mut PrintItems) {
		if !self.is_empty() {
			let start = LineNumber::new("args start");
			let end = LineNumber::new("args end");
			out.push_str("(");
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
			for arg in self.unnamed.iter() {
				if first.take().is_none() {
					out.push_str(",");
					out.push_condition(arg_nl_condition.clone());
				}
				arg.print(out, None);
			}
			for (k, v) in self.named.iter() {
				if first.take().is_none() {
					out.push_str(",");
					out.push_condition(arg_nl_condition.clone());
				}
				out.push_string(k.0.to_string());
				out.push_str(": ");
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
			out.push_str(")");
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
			out.push_str(" ");
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
			indent_width: 4,
			use_tabs: true,
			new_line_text: "\n",
		},
	)
}
