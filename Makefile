PREFIX = /usr/local
MANPREFIX = ${PREFIX}/share/man

SRC != find src -name '*.rs'

all: fuse-ufs

install: fuse-ufs
	mkdir -p ${DESTDIR}${PREFIX}/bin ${DESTDIR}${MANPREFIX}/man8
	cp -f fuse-ufs ${DESTDIR}${PREFIX}/bin/
	cp -f docs/fuse-ufs.8 ${DESTDIR}${MANPREFIX}/man8/
	ln -sf fuse-ufs ${DESTDIR}${PREFIX}/bin/mount.ufs

prepare: fmt lint

fmt:
	cargo +nightly fmt
	./scripts/fmt-changelog.sh

lint:
	cargo clippy --all-targets

clean:
	rm -f fuse-ufs
	cargo clean

# TODO: select correct program
fuse-ufs: Cargo.lock ${SRC}
	cargo build --release
	cp -f target/release/fuse-ufs-fuser fuse-ufs

