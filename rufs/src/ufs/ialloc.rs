use super::*;
use crate::{err, InodeNum};

impl<R: Backend> Ufs<R> {
	pub(super) fn inode_free(&mut self, inr: InodeNum) -> IoResult<()> {
		self.assert_rw()?;
		let mut ino = self.read_inode(inr)?;
		ino.nlink -= 1;
		self.write_inode(inr, &ino)?;

		if ino.nlink > 0 {
			return Ok(());
		}

		let sb = &self.superblock;

		// clear the inode
		let off = self.superblock.ino_to_fso(inr);
		self.file.fill_at(off, 0u8, UFS_INOSZ)?;
		
		// calculate the cylinder group number and offset for the inode.
		let (cgi, cgo) = self.superblock.ino_in_cg(inr);
		let cga = self.cg_addr(cgi);
		let mut cg: CylGroup = self.file.decode_at(cga)?;

		if cg.magic != CG_MAGIC {
			panic!("inode_free({inr}): invalid cylinder group: cgi={cgi}, cga={cga:08x}");
		}

		// free the inode in the inode bitmap
		let off = cga + cg.iusedoff as u64 + cgo / 8;
		let mut b: u8 = self.file.decode_at(off)?;
		let mask = 1 << (cgo % 8);
		if (b & mask) != mask {
			panic!("inode_free({inr}): double-free: cgi={cgi}, cgo={cgo}, cga={cga:08x}, off={off:08x}, b={b:02x}, iusedoff={:02x}, ipg={}", cg.iusedoff, sb.ipg);
		}
		b &= !mask;
		self.file.encode_at(off, &b)?;

		// update the CG's free inode count
		cg.cs.nifree += 1;
		// TODO: update cg.time
		self.file.encode_at(cga, &cg)?;
		
		Ok(())
	}
}
