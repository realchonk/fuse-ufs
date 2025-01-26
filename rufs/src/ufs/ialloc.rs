use super::*;
use crate::{err, InodeNum};

impl<R: Backend> Ufs<R> {
	pub(super) fn inode_alloc(&mut self) -> IoResult<InodeNum> {
		self.assert_rw()?;
		let sb = &self.superblock;
		let ipg = sb.ipg as u64;
		assert_eq!(ipg % 8, 0);

		for cgi in 0..sb.ncg {
			let cga = self.cg_addr(cgi.into());
			let mut cg: CylGroup = self.file.decode_at(cga)?;

			if cg.cs.nifree <= 0 {
				continue;
			}

			let off = cga + cg.iusedoff as u64;
			let end = off + ipg / 8;

			for i in off..end {
				let mut b: u8 = self.file.decode_at(i)?;
				if b == 0xff {
					continue;
				}

				let j = b.leading_ones();
				assert!(j < 8);
				b |= 1 << j;
				self.file.encode_at(i, &b)?;

				let inr = cgi as u64 * ipg + i * 8 + j as u64;
				let inr = unsafe { InodeNum::new(inr as u32) };

				let mut ino = self.read_inode(inr)?;
				if ino.nlink != 0 {
					panic!("inode_alloc(): use after free: inr={inr}, cgi={cgi}, cga={cga:08x}, i={i}, j={j}, ipg={ipg}");
				}
				ino.nlink = 1;
				self.write_inode(inr, &ino)?;

				// update free count in CG
				cg.cs.nifree -= 1;
				self.file.encode_at(cga, &cg)?;

				self.update_sb(|sb| sb.cstotal.nifree -= 1)?;

				return Ok(inr);
			}
		}

		Err(err!(ENOSPC))
	}

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

		self.update_sb(|sb| sb.cstotal.nifree += 1)?;

		// TODO: free data blocks

		Ok(())
	}
}
