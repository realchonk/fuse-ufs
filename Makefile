PREFIX = /usr/local

SRC != find src -name '*.rs'

all: fuse-ufs

install: fuse-ufs
	mkdir -p ${DESTDIR}${PREFIX}/bin
	cp -f fuse-ufs ${DESTDIR}${PREFIX}/bin/

run: fuse-ufs
	(RUST_BACKTRACE=full RUST_LOG=debug ./fuse-ufs /dev/da0 mp & { sleep 2; ls -l mp; cat mp/file1; sleep 3; umount mp; })

prepare: fmt lint

fmt:
	cargo +nightly fmt

lint:
	cargo clippy --all-targets

clean:
	rm -f fuse-ufs
	cargo clean

mount-test:
	mkdir -p mp
	(cd resources; unzstd -k ufs.img.zst)
	mount -t ufs /dev/`mdconfig -f resources/ufs.img` mp

umount-test:
	umount mp
	mdconfig -d -u 0
	(cd resources; zstd ufs.img)

fuse-ufs: Cargo.lock ${SRC}
	cargo build --release
	cp -f target/release/fuse-ufs .

