PREFIX = /usr/local
MANPREFIX = ${PREFIX}/share/man
FUSE_UFS_FLAGS = -p fuse-ufs --ignore-rust-version --no-default-features -F $$(uname)

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
	cargo test -p rufs --ignore-rust-version
	cargo test ${FUSE_UFS_FLAGS}

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
	RUST_BACKTRACE=1 cargo r -p fuse-ufs -- -vvvforw,allow_other resources/ufs-little.img mp

clean:
	rm -f fuse-ufs-bin
	cargo clean
	find . -name '*.core' -delete -print
	find . -name '*.orig' -delete -print
	find . -name '*.rej' -delete -print
	rm -f .patch

fuse-ufs-bin: Cargo.lock ${SRC}
	cargo build --release ${FUSE_UFS_FLAGS}
	cp -f target/release/fuse-ufs fuse-ufs-bin

