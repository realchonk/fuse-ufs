
SRC != find src -name '*.rs'

all: fuse-ufs

prepare: fmt lint

fmt:
	cargo fmt

lint:
	cargo clippy

clean:
	rm -f fuse-ufs
	cargo clean


fuse-ufs: ${SRC}
	cargo build --release
	cp -f target/release/fuse-ufs .

