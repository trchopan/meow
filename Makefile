SHELL := /bin/sh

.PHONY: fmt lint test build check dev-smoke

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

build:
	cargo build

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	cargo build

dev-smoke:
	cargo run -- dev-smoke --duration-secs 5
