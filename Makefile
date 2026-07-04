# zdb — build / test / cross-package.
#
# Dev is on WSL2 aarch64 Linux. Windows-on-ARM64 is the ship target and cross-
# compiles here (llvm-mingw). macOS must be built ON a Mac (no macOS SDK here) —
# the `macos*` targets are provided for that, and `sqls` already cross-builds to
# every arch via Go.
#
# Common: `make build` `make test` `make run` `make windows` `make package-windows`

CARGO        ?= $(HOME)/.cargo/bin/cargo
LLVM_MINGW   ?= $(HOME)/.local/llvm-mingw
WIN_ARM       := aarch64-pc-windows-gnullvm
WIN_X64       := x86_64-pc-windows-gnu
ZDBTEST       ?= /mnt/c/zdbtest

# llvm-mingw env for the aarch64 Windows cross-build (see CLAUDE.md).
WIN_ARM_ENV = \
	PATH="$(LLVM_MINGW)/bin:$$PATH" \
	CC_aarch64_pc_windows_gnullvm=aarch64-w64-mingw32-clang \
	CXX_aarch64_pc_windows_gnullvm=aarch64-w64-mingw32-clang++ \
	AR_aarch64_pc_windows_gnullvm=llvm-ar \
	CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER=aarch64-w64-mingw32-clang

.PHONY: all build run test clippy fmt sqls dev-sqls \
        windows windows-x64 package-windows deploy-windows-test \
        macos macos-arm macos-x64 package-macos clean help

all: build

## --- local dev (Linux) ---------------------------------------------------

build: ## Debug build of the app
	$(CARGO) build -p zdb-app

run: build dev-sqls ## Build + run the app (dev)
	./target/debug/zdb

test: ## Run all tests
	$(CARGO) test

clippy: ## Lint
	$(CARGO) clippy -p zdb-app -p zdb-db

fmt: ## Format
	$(CARGO) fmt

## --- sqls language server (Go, all arches) --------------------------------

sqls: ## Cross-build sqls for every arch into dist/sqls/
	bash scripts/build-sqls.sh

dev-sqls: dist/sqls/sqls-linux-arm64 ## Stage the dev sqls next to the debug exe
	@cp dist/sqls/sqls-linux-arm64 target/debug/sqls 2>/dev/null || true

dist/sqls/sqls-linux-arm64:
	bash scripts/build-sqls.sh

## --- Windows (cross-compiled here) ----------------------------------------

windows: ## Cross-build zdb.exe for Windows ARM64 (release)
	$(WIN_ARM_ENV) $(CARGO) build -p zdb-app --release --target $(WIN_ARM)

windows-x64: ## Cross-build zdb.exe for Windows x64 (release)
	$(CARGO) build -p zdb-app --release --target $(WIN_X64)

package-windows: windows sqls ## Build + bundle the Windows ARM64 zip (exe + sqls + shaders)
	bash scripts/package-windows.sh $(WIN_ARM) arm64

deploy-windows-test: package-windows ## Copy the unpacked Windows ARM64 build to $(ZDBTEST)
	mkdir -p "$(ZDBTEST)"
	cp -r dist/zdb-windows-arm64/. "$(ZDBTEST)/"
	@echo "deployed to $(ZDBTEST)"

## --- macOS (must run ON a Mac; no SDK on the Linux dev box) ----------------

macos: macos-arm ## Alias: native macOS build (Apple Silicon)

macos-arm: ## Native release build for Apple Silicon (run on a Mac)
	$(CARGO) build -p zdb-app --release --target aarch64-apple-darwin

macos-x64: ## Native release build for Intel macOS (run on a Mac)
	$(CARGO) build -p zdb-app --release --target x86_64-apple-darwin

package-macos: sqls ## Assemble zdb.app for the host arch + bundle sqls (run on a Mac)
	bash scripts/package-macos.sh

## -------------------------------------------------------------------------

clean: ## Remove build + dist artifacts
	$(CARGO) clean
	rm -rf dist

help: ## List targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
