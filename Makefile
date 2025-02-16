PREFIX = /usr/local
MANPREFIX = ${PREFIX}/share/man

SRC != find rufs/src fuse-ufs/src -name '*.rs'

all: fuse-ufs-bin

install: fuse-ufs-bin
	mkdir -p ${DESTDIR}${PREFIX}/bin ${DESTDIR}${MANPREFIX}/man8
	cp -f fuse-ufs-bin ${DESTDIR}${PREFIX}/bin/fuse-ufs
	cp -f docs/fuse-ufs.8 ${DESTDIR}${MANPREFIX}/man8/
	ln -sf fuse-ufs ${DESTDIR}${PREFIX}/bin/mount.ufs

prepare: fmt lint

fmt:
	cargo +nightly fmt
	./scripts/fmt-changelog.sh

lint:
	cargo clippy --all-targets

test:
	cargo test -p rufs
	cargo test -p fuse-ufs --no-default-features --features "$$(uname)"

fuz:
	mkdir -p fuzz/corpus/ufs/
	unzstd -o fuzz/corpus/ufs/ufs-big.img -kf resources/ufs-big.img.zst
	unzstd -o fuzz/corpus/ufs/ufs-little.img -kf resources/ufs-little.img.zst
	# NOTE: Add -j if you want more fuzz jobs
	cargo +nightly fuzz run ufs

mount:
	mkdir -p mp
	rm -f resources/ufs-little.img
	unzstd -k resources/ufs-little.img.zst
	cargo r -p fuse-ufs -- -vvvforw,allow_other resources/ufs-little.img mp

clean:
	rm -f fuse-ufs-bin
	cargo clean

fuse-ufs-bin: Cargo.lock ${SRC}
	cargo build --release -p fuse-ufs --no-default-features --features "$$(uname)"
	cp -f target/release/fuse-ufs fuse-ufs-bin

