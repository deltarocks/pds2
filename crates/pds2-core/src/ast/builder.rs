use super::{Block, Ident, Item, ItemArgs, Path, Value};

pub struct ItemArgsBuilder {
	args: ItemArgs,
}
impl ItemArgsBuilder {
	pub fn arg(mut self, value: Value) -> Self {
		self.args.push(None, value);
		self
	}
	pub fn named(mut self, name: impl Into<Ident>, value: Value) -> Self {
		if !self.args.push(Some(name.into()), value) {
			panic!("named set twice");
		}
		self
	}
	pub fn build(self) -> ItemArgs {
		self.args
	}
}

impl ItemArgs {
	pub fn builder() -> ItemArgsBuilder {
		ItemArgsBuilder {
			args: ItemArgs::default(),
		}
	}
}

pub struct ItemBuilder {
	item: Item,
}

impl ItemBuilder {
	pub fn argsb(mut self, args: ItemArgsBuilder) -> Self {
		self.item.args = args.build();
		self
	}
	pub fn args(mut self, args: ItemArgs) -> Self {
		self.item.args = args;
		self
	}
	pub fn block(mut self, block: Block) -> Self {
		self.item.block = block;
		self
	}
	pub fn build(self) -> Item {
		self.item
	}
	pub fn then(self, mut value: Vec<Value>) -> Vec<Value> {
		value.insert(0, self.build().into());
		value
	}
	pub fn build_values(self) -> Vec<Value> {
		vec![self.build().into()]
	}
}

impl Item {
	pub fn simple_builder(name: impl Into<Ident>) -> ItemBuilder {
		ItemBuilder {
			item: Item::new_simple(name),
		}
	}
	pub fn path_builder(name: Path) -> ItemBuilder {
		ItemBuilder {
			item: Item {
				name,
				args: ItemArgs::default(),
				block: Block::default(),
			},
		}
	}
}
