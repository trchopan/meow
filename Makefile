SHELL := /bin/sh

APP_NAME := Meow.app
APP_VERSION ?= $(shell awk -F'"' '/^version = / { print $$2; exit }' Cargo.toml)
APP_BUILD_DIR ?= dist/$(APP_NAME)
INSTALL_DIR ?= $(HOME)/Applications
INSTALL_APP := $(INSTALL_DIR)/$(APP_NAME)
CLI_INSTALL_DIR ?= $(HOME)/.local/bin

.PHONY: fmt lint test build check dev-smoke app app-check install-app launch-app uninstall-app install-cli doctor diagnose logs reset-tcc help

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

build:
	cargo build --all-targets

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	cargo build --all-targets

dev-smoke:
	cargo run -- dev-smoke --duration-secs 5

app:
	cargo build --release --bins
	rm -rf "$(APP_BUILD_DIR)"
	mkdir -p "$(APP_BUILD_DIR)/Contents/MacOS"
	cp "target/release/meow-menubar" "$(APP_BUILD_DIR)/Contents/MacOS/meow-menubar"
	cp "target/release/meow" "$(APP_BUILD_DIR)/Contents/MacOS/meow"
	sed "s/@VERSION@/$(APP_VERSION)/g" resources/macos/Info.plist > "$(APP_BUILD_DIR)/Contents/Info.plist"
	chmod +x "$(APP_BUILD_DIR)/Contents/MacOS/meow-menubar" "$(APP_BUILD_DIR)/Contents/MacOS/meow"
	codesign --force --deep -s - "$(APP_BUILD_DIR)"

app-check: app
	plutil -lint "$(APP_BUILD_DIR)/Contents/Info.plist"
	test -x "$(APP_BUILD_DIR)/Contents/MacOS/meow-menubar"
	test "$$(plutil -extract CFBundleExecutable raw "$(APP_BUILD_DIR)/Contents/Info.plist")" = "meow-menubar"
	test "$$(plutil -extract CFBundlePackageType raw "$(APP_BUILD_DIR)/Contents/Info.plist")" = "APPL"
	codesign -v "$(APP_BUILD_DIR)"
	printf 'validated %s\n' "$(APP_BUILD_DIR)"

install-app: app-check
	mkdir -p "$(INSTALL_DIR)"
	rm -rf "$(INSTALL_APP)"
	cp -R "$(APP_BUILD_DIR)" "$(INSTALL_APP)"
	printf 'installed %s\n' "$(INSTALL_APP)"

launch-app:
	test -d "$(INSTALL_APP)"
	open "$(INSTALL_APP)"

uninstall-app:
	rm -rf "$(INSTALL_APP)"
	printf 'removed %s\n' "$(INSTALL_APP)"

install-cli:
	cargo build --release --bin meow
	mkdir -p "$(CLI_INSTALL_DIR)"
	install -m 755 target/release/meow "$(CLI_INSTALL_DIR)/meow"
	printf 'installed %s\n' "$(CLI_INSTALL_DIR)/meow"

doctor:
	cargo run -- doctor

diagnose:
	cargo run -- diagnose

logs:
	tail -n 100 -f "$(HOME)/.local/share/meow/logs/meow.log"

reset-tcc:
	tccutil reset Accessibility com.meow.inputsharing || true
	tccutil reset ListenEvent com.meow.inputsharing || true
	printf 'reset TCC permissions for com.meow.inputsharing\n'

help:
	printf '%s\n' \
		'make check        Run formatting, lint, tests, and all-target build' \
		'make dev-smoke    Run the isolated host/client smoke test' \
		'make app          Build dist/Meow.app from release binaries with ad-hoc signing' \
		'make app-check    Build and validate the local app bundle' \
		'make install-app  Install Meow.app into ~/Applications' \
		'make launch-app   Launch ~/Applications/Meow.app' \
		'make uninstall-app Remove ~/Applications/Meow.app' \
		'make install-cli  Install the meow CLI into ~/.local/bin' \
		'make doctor       Run environment, permissions, and tap health checks' \
		'make diagnose     Export a sanitized diagnostics bundle (.zip)' \
		'make logs         Follow real-time persistent log output' \
		'make reset-tcc    Reset macOS TCC permissions for clean testing'
