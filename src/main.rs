use std::{ffi::c_int, path::Path, thread::sleep, time::Duration};

use fuser::{Filesystem, KernelConfig, Request};


pub struct Ufs;

impl Ufs {
    pub fn new() -> Self {
	Self
    }
}

impl Filesystem for Ufs {
    fn init(&mut self, _req: &Request<'_>, _config: &mut KernelConfig) -> Result<(), c_int> {
	println!("init()");
	Ok(())
    }
    fn destroy(&mut self) {
	println!("destroy()");
    }
}

fn main() -> std::io::Result<()> {
    let fs = Ufs::new();
    let mp = Path::new("mp");
    let options = &[];
    let mount = fuser::spawn_mount2(fs, mp, options)?;
    sleep(Duration::new(1, 0));
    drop(mount);
    Ok(())
}
