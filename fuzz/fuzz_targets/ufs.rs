#![no_main]

use std::io::{Cursor, Read, Seek, Write};

use libfuzzer_sys::fuzz_target;
use rufs::*;

fuzz_target!(|data: Vec<u8>| {
	let rdr = BlockReader::new(Cursor::new(data), 4096, false);
	let mut fs = match Ufs::new(rdr) {
		Ok(fs) => fs,
		// Malformed FS already detected and handled properly by rufs
		Err(_) => return,
	};
	traverse(&mut fs, InodeNum::ROOT);
});

fn traverse<R: Read + Write + Seek>(fs: &mut Ufs<R>, inr: InodeNum) {
	let mut children = Vec::new();
	let _ = fs.dir_iter(inr, |name, inr, kind| {
		children.push((name.to_owned(), inr, kind));
		None::<()>
	});
	for (_name, cinr, _kind) in children {
		// TODO: Also excercise other fs methods
		traverse(fs, cinr);
	}
}
