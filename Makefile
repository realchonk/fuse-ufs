
SRC != find src -name '*.rs'

all: fuse-ufs

run: fuse-ufs
	RUST_BACKTRACE=full RUST_LOG=debug ./fuse-ufs /dev/da0

prepare: fmt lint

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets

clean:
	rm -f fuse-ufs
	cargo clean


fuse-ufs: Cargo.lock ${SRC}
	cargo build --release
	cp -f target/release/fuse-ufs .

