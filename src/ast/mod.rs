use anyhow::{anyhow, bail, ensure, Context, Result};
use genco::{lang::Rust, prelude::*, tokens::FormatInto};
use std::{
	cell::RefCell,
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	convert::TryFrom,
	fmt::Debug,
	iter,
	rc::Rc,
};
mod format;
pub use format::format;
mod parse;
pub use parse::parse;

pub mod builder;

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ident(Rc<str>);
impl Ident {
	pub fn new(v: &str) -> Self {
		Self(v.into())
	}
	pub fn as_str(&self) -> &str {
		&self.0
	}
	pub fn is_type_arg(&self) -> bool {
		self.0
			.chars()
			.next()
			.map(|c| c.is_uppercase())
			.unwrap_or(false)
	}
}
impl std::fmt::Display for Ident {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.0)
	}
}
impl FormatInto<Rust> for Ident {
	fn format_into(self, tokens: &mut rust::Tokens) {
		quote_in!(*tokens => $(self.0.as_ref()))
	}
}
impl FormatInto<Rust> for &Ident {
	fn format_into(self, tokens: &mut rust::Tokens) {
		quote_in!(*tokens => $(self.0.as_ref()))
	}
}
impl Debug for Ident {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let has_split = self.0.contains("::");
		if has_split {
			write!(f, "\"")?;
		}
		write!(f, "{}", self.0)?;
		if has_split {
			write!(f, "\"")?;
		}
		Ok(())
	}
}
impl From<&str> for Ident {
	fn from(s: &str) -> Self {
		Self(s.into())
	}
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path {
	pub absolute: bool,
	pub path: Vec<Ident>,
	pub item: Ident,
}

impl Path {
	pub fn new(mut segments: Vec<Ident>) -> Self {
		let item = segments.pop().expect("name");
		Self {
			absolute: true,
			path: segments,
			item,
		}
	}
	pub fn new_simple(v: Ident) -> Self {
		Path {
			absolute: false,
			path: vec![],
			item: v,
		}
	}
	pub fn simple_name(&self) -> Option<Ident> {
		(!self.absolute && self.path.is_empty()).then(|| self.item.clone())
	}
	pub fn expect_simple_name(&self) -> Result<Ident> {
		self.simple_name()
			.ok_or_else(|| anyhow!("expected non-namespaced name"))
	}
	pub fn rewrite_to_absolute(
		&mut self,
		current_ns: &[Ident],
		alternatives: &HashSet<Vec<Ident>>,
	) {
		if !self.absolute {
			let mut new_path = current_ns.to_vec();
			'found: {
				if !new_path.is_empty() {
					let extension_len = self.path.len();
					while !new_path.is_empty() {
						new_path.extend(self.path.iter().cloned());
						new_path.push(self.item.clone());
						if alternatives.contains(&new_path) {
							// item name
							new_path.pop();
							break 'found;
						}
						// extension
						for _ in 0..extension_len {
							new_path.pop();
						}
						// last ns segment
						new_path.pop();
						// item name
						new_path.pop();
					}
					// Try root
					new_path = vec![];
					new_path.append(&mut self.path);
					new_path.push(self.item.clone());
					if alternatives.contains(&new_path) {
						// item name
						new_path.pop();
						break 'found;
					}

					// Not found - should fail on some other point
					new_path = current_ns.to_vec();
					new_path.append(&mut self.path);
				} else {
					new_path.append(&mut self.path);
				}
			}
			self.path = new_path;
			self.absolute = true;
		}
	}
	pub fn readable_name(&self) -> String {
		self.path
			.iter()
			.chain(iter::once(&self.item))
			.map(|p| p.as_str())
			.collect::<Vec<_>>()
			.join("_")
	}
}

impl Debug for Path {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.absolute {
			write!(f, "root::")?;
		}
		for item in self.path.iter() {
			write!(f, "{:?}::", item)?;
		}
		write!(f, "{:?}", self.item)?;
		Ok(())
	}
}

#[derive(Debug, Clone, Default)]
pub struct Block {
	pub attributes: Attributes,
	pub statements: Vec<Statement>,
	pub root: bool,
}
impl Block {
	pub fn is_empty(&self) -> bool {
		self.statements.is_empty()
	}
}

pub type Attributes = HashMap<Ident, Vec<Value>>;

#[derive(Debug, Clone)]
pub struct Statement {
	pub attributes: Attributes,
	pub items: Vec<Value>,
}

impl Statement {
	pub fn simple(items: Vec<Value>) -> Self {
		Self {
			attributes: Default::default(),
			items,
		}
	}
	pub fn pop_item(self) -> Result<(Attributes, Item, Vec<Value>)> {
		let (attrs, value, values) = self.pop_value();
		let item = value.into_item().ok_or_else(|| anyhow!("expected item"))?;
		Ok((attrs, item, values))
	}
	fn pop_value(self) -> (Attributes, Value, Vec<Value>) {
		let Statement {
			attributes,
			mut items,
		} = self;
		// Statement always has at least one item
		let first = items.remove(0);
		(attributes, first, items)
	}
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FieldRefSegment {
	Down { to: Ident, ignore_delegation: bool },
	Virtual(Ident),
	Arg(Ident),
}
impl FieldRefSegment {
	fn name(&self) -> String {
		match self {
			FieldRefSegment::Down { to, .. } => to.as_str().to_string(),
			FieldRefSegment::Virtual(v) => v.as_str().to_string(),
			FieldRefSegment::Arg(a) => a.as_str().to_string(),
		}
	}
}
impl Debug for FieldRefSegment {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			FieldRefSegment::Down { to, .. } => write!(f, "{:?}", to),
			FieldRefSegment::Virtual(v) => write!(f, "@{:?}", v),
			FieldRefSegment::Arg(a) => write!(f, "${:?}", a),
		}
	}
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BaseFieldRef(pub Vec<FieldRefSegment>);
impl BaseFieldRef {
	pub fn name(&self) -> String {
		self.0
			.iter()
			.map(|p| p.name())
			.collect::<Vec<_>>()
			.join("_")
	}
}
impl TryFrom<FieldRef> for BaseFieldRef {
	type Error = anyhow::Error;

	fn try_from(value: FieldRef) -> Result<Self, Self::Error> {
		ensure!(value.up == 0, "not a based reference: {:?}", value);
		Ok(value.path)
	}
}
impl Debug for BaseFieldRef {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		for (i, item) in self.0.iter().enumerate() {
			if i != 0 {
				write!(f, "/")?;
			}
			write!(f, "{:?}", item)?;
		}
		Ok(())
	}
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldRef {
	pub up: usize,
	pub path: BaseFieldRef,
}
impl FieldRef {
	pub fn name(&self) -> String {
		self.path.name()
	}
	pub fn parse(s: &str) -> Result<Self> {
		Ok(parse::parse_reference(s)?)
	}
}
impl Debug for FieldRef {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.up == 0 {
			write!(f, ".")?
		} else {
			for i in 0..self.up {
				if i != 0 {
					write!(f, "/")?;
				}
				write!(f, "..")?;
			}
		}
		write!(f, "/{:?}", self.path)
	}
}

#[derive(Debug, Clone)]
pub enum Value {
	String { string: String, is_block: bool },
	FieldRef(FieldRef),
	FunctionCall(Ident, ItemArgs),
	Int(i128),
	Item(Item),
	Internal(String),
	Unset,
}

impl From<Item> for Value {
	fn from(i: Item) -> Self {
		Self::Item(i)
	}
}
impl From<i128> for Value {
	fn from(i: i128) -> Self {
		Self::Int(i)
	}
}
impl From<u32> for Value {
	fn from(i: u32) -> Self {
		Self::Int(i as i128)
	}
}
impl From<FieldRef> for Value {
	fn from(r: FieldRef) -> Self {
		Self::FieldRef(r)
	}
}

#[derive(Debug, Clone, Default)]
pub struct ItemArgs {
	unnamed: Vec<Value>,
	named: BTreeMap<Ident, Value>,
	used_unnamed: RefCell<BTreeSet<usize>>,
	used_named: RefCell<BTreeSet<Ident>>,
}
impl ItemArgs {
	fn empty() -> Self {
		Self::default()
	}
	pub fn is_empty(&self) -> bool {
		self.unnamed.is_empty() && self.named.is_empty()
	}
	pub fn arg<T: ParseValue>(&self, id: usize, name: &str) -> Result<T> {
		if self.unnamed.len() > id {
			self.used_unnamed.borrow_mut().insert(id);
		}
		let value = self.unnamed.get(id).cloned().unwrap_or(Value::Unset);
		T::parse(value).with_context(|| format!("parsing positional argument {} ({})", id, name))
	}
	pub fn named<T: ParseValue>(&self, name: impl Into<Ident>) -> Result<T> {
		let name: Ident = name.into();
		if self.named.contains_key(&name) {
			self.used_named.borrow_mut().insert(name.clone());
		}
		let value = self.named.get(&name).cloned().unwrap_or(Value::Unset);
		T::parse(value).with_context(|| format!("parsing named argument {:?}", name))
	}
	pub fn either<A: ParseValue, B: ParseValue>(
		&self,
		name_a: impl Into<Ident>,
		name_b: impl Into<Ident>,
	) -> Result<Result<A, B>> {
		let name_a: Ident = name_a.into();
		Ok(if self.named.contains_key(&name_a) {
			Ok(self.named(name_a).context("either arg left")?)
		} else {
			Err(self.named(name_b).context("either arg right")?)
		})
	}
	pub fn named_names<'i>(&'i self) -> impl Iterator<Item = &'i Ident> {
		self.named.keys()
	}
	pub fn verify_unused(&self) -> Result<()> {
		for i in 0..self.unnamed.len() {
			if !self.used_unnamed.borrow().contains(&i) {
				bail!("unused argument {}: {:?}", i, self.unnamed[i]);
			}
		}
		for i in self.named.keys() {
			if !self.used_named.borrow().contains(i) {
				bail!("unused argument {:?}: {:?}", i, self.named[i]);
			}
		}
		Ok(())
	}
}

pub trait ParseValue: Sized {
	fn parse(value: Value) -> Result<Self>;
}
impl<T: ParseValue> ParseValue for Option<T> {
	fn parse(value: Value) -> Result<Self> {
		if matches!(value, Value::Unset) {
			Ok(None)
		} else {
			Ok(Some(T::parse(value)?))
		}
	}
}
impl ParseValue for bool {
	fn parse(value: Value) -> Result<Self> {
		if value.is_unset() {
			bail!("missing boolean")
		}
		let item = value
			.into_item()
			.ok_or_else(|| anyhow!("expected boolean"))?;
		let v = item
			.simple_name()
			.ok_or_else(|| anyhow!("expected boolean"))?;
		match v.as_str() {
			"true" => Ok(true),
			"false" => Ok(false),
			v => bail!("expected boolean, got {}", v),
		}
	}
}
impl ParseValue for i128 {
	fn parse(value: Value) -> Result<Self> {
		if value.is_unset() {
			bail!("missing number")
		}
		if let Some(v) = value.clone().into_number() {
			return Ok(v);
		}
		if let Some(s) = value.string() {
			let (base, s) = if let Some(s) = s.strip_prefix("0x") {
				(16, s)
			} else {
				(10, s)
			};
			return i128::from_str_radix(s, base)
				.with_context(|| format!("invalid string-encoded number {s:?} for base {base:?}"));
		}
		bail!("expected number (or string-encoded number)")
	}
}
impl ParseValue for Ident {
	fn parse(value: Value) -> Result<Self> {
		if value.is_unset() {
			bail!("missing identifier")
		}
		if let Value::Int(v) = value {
			// deprecated
			return Ok(format!("v{}", v).as_str().into());
		}
		let item = value
			.into_item()
			.ok_or_else(|| anyhow!("expected identifer, got nothing"))?;
		item.simple_name()
			.ok_or_else(|| anyhow!("expected identifer"))
	}
}
impl ParseValue for FieldRef {
	fn parse(value: Value) -> Result<Self> {
		if value.is_unset() {
			bail!("missing reference");
		}
		match value {
			Value::FieldRef(v) => Ok(v),
			_ => bail!("expected field reference, got {value:?}"),
		}
	}
}
impl ParseValue for Value {
	fn parse(value: Value) -> Result<Self> {
		Ok(value)
	}
}
impl ParseValue for Item {
	fn parse(value: Value) -> Result<Self> {
		if value.is_unset() {
			bail!("missing item")
		}
		match value {
			Value::Item(i) => Ok(i),
			_ => bail!("expected item"),
		}
	}
}

#[derive(Debug, Clone)]
pub struct Item {
	pub name: Path,
	pub args: ItemArgs,
	pub block: Block,
}

impl Item {
	pub fn new_simple(name: impl Into<Ident>) -> Self {
		Self {
			name: Path::new_simple(name.into()),
			args: ItemArgs::empty(),
			block: Block::default(),
		}
	}
	pub fn simple_name(&self) -> Option<Ident> {
		(self.args.is_empty() && self.block.is_empty())
			.then(|| self.name.simple_name())
			.flatten()
	}
}

// Helpers
impl Value {
	pub fn item<'a>(&'a self) -> Option<&'a Item> {
		if let Value::Item(item) = self {
			Some(item)
		} else {
			None
		}
	}

	pub fn into_item(self) -> Option<Item> {
		if let Value::Item(item) = self {
			Some(item)
		} else {
			None
		}
	}

	pub fn into_number(self) -> Option<i128> {
		if let Value::Int(v) = self {
			Some(v)
		} else {
			None
		}
	}

	pub fn string<'a>(&'a self) -> Option<&'a str> {
		if let Value::String { string, .. } = self {
			Some(string)
		} else {
			None
		}
	}

	pub fn reference(&self) -> Option<FieldRef> {
		if let Value::FieldRef(r) = self {
			Some(r.clone())
		} else {
			None
		}
	}

	pub fn new_string(string: String) -> Value {
		Value::String {
			string,
			is_block: false,
		}
	}

	pub fn new_simple_item(name: impl Into<Ident>) -> Value {
		Value::Item(Item::new_simple(name))
	}

	pub fn is_unset(&self) -> bool {
		matches!(self, Self::Unset)
	}
}
