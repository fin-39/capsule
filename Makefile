.PHONY: all check test release run appimage clean

all: check

check:
	cargo fmt --check
	cargo check --all-targets

test:
	cargo test

release:
	cargo build --release --bins

run:
	cargo run

appimage:
	./scripts/build-appimage.sh

clean:
	cargo clean
