.DEFAULT_GOAL := help
.PHONY: help check format static static-test systemd-fixture package verify-release distros

RELEASE_DIR ?= dist
COMMIT ?= $(shell git rev-parse HEAD)
RUFF ?= ruff

help:
	@printf '%s\n' \
	  'make check                         Format, lint, unit tests and offline checks' \
	  'make format                        Format Rust source' \
	  'make static                        Build and verify the static executable' \
	  'make static-test                   Also run the musl tests' \
	  'make systemd-fixture               Build the separate static VM test executable' \
	  'make package                       Package a clean commit into dist/' \
	  'make verify-release                Verify dist/ against the current commit' \
	  'make distros [DISTROS="debian ..."]  Run disposable container checks'

check:
	$(RUFF) check .
	$(RUFF) format --check .
	cargo fmt --all -- --check
	cargo clippy --locked --bin minibookx-ctl -- -D warnings
	cargo clippy --locked --all-targets --features systemd-test-fixture,dev-tools -- -D warnings
	cargo test --locked --bin minibookx-ctl
	cargo test --locked --features systemd-test-fixture,dev-tools
	PYTHONPATH=scripts python3 -m unittest discover -s tests/tooling -p 'test_*.py'
	python3 tests/tooling/verify_units.py
	python3 tests/tooling/check_docs.py
	shellcheck install.sh scripts/*.sh tests/containers/*.sh

format:
	cargo fmt --all

static:
	./scripts/build-static.sh

static-test:
	./scripts/build-static.sh --test

systemd-fixture:
	./scripts/build-static.sh --fixture

package:
	python3 scripts/package_release.py

verify-release:
	python3 scripts/release_format.py check "$(RELEASE_DIR)" --commit "$(COMMIT)"

distros:
	./tests/containers/run.sh $(DISTROS)
