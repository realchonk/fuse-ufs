use std::time::SystemTime;

pub type Inode = u64;

pub struct Request {
	pub uid: u32,
	pub gid: u32,
}

pub enum FileType {
	RegularFile,
	Directory,
	NamedPipe,
	Socket,
	CharDevice,
	BlockDevice,
	Symlink,
}

pub struct FileAttr {
	pub size: u64,
	pub blocks: u64,
	pub atime: SystemTime,
	pub mtime: SystemTime,
	pub ctime: SystemTime,
	pub btime: SystemTime,
	pub kind: FileType,
	pub perm: u16,
	pub uid: u32,
	pub gid: u32,
	pub rdev: u32,
	pub blksize: u32,
	pub flags: u32,
	pub nlink: u32,
}

pub struct FileInfo {
	pub flags: i32,
	pub fh: u64,
}
