use crate::{Input, Output, Result};

pub mod pds_void {
	use super::*;
	pub type Ty = ();
	pub fn serialize(_0: &Ty, _output: &mut Output) -> Result<()> {
		Ok(())
	}
	pub fn deserialize(_input: &mut Input) -> Result<Ty> {
		Ok(())
	}
	pub fn size_of(_0: &Ty) -> usize {
		0
	}
}
