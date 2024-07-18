PREFIX = /usr/local

SRC != find src -name '*.rs'

all: fuse-ufs

install: fuse-ufs
	mkdir -p ${DESTDIR}${PREFIX}/bin
	cp -f fuse-ufs ${DESTDIR}${PREFIX}/bin/

prepare: fmt lint

fmt:
	cargo +nightly fmt

lint:
	cargo clippy --all-targets

clean:
	rm -f fuse-ufs
	cargo clean

fuse-ufs: Cargo.lock ${SRC}
	cargo build --release
	cp -f target/release/fuse-ufs .

