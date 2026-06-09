use crate::pds::{Input, Output};

mod pds;
mod types;

#[test]
fn test() {
	use types::pds_protocol_testMap::Ty;

	// let pkt: pktt::Ty = pktt::Ty {
	// 	// data: vec![],
	// 	data: vec![1, 2, 3, 4],
	// };

	// let mut buf = vec![0u8; pktt::size_of(&pkt)];
	// let mut out = Output::new(&mut buf);

	// pktt::serialize(&pkt, &mut out).expect("serialize");

	// let pkt2 = pktt::deserialize(&mut Input::new(&buf)).expect("deserialize");

	// assert_eq!(pkt.size, pkt2.size);
}
