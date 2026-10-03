# Cargo profile for build, run and test: dev (default) or release.
PROFILE ?= dev
# Extra arguments for `make run`, e.g. ARGS="--events my.jsonl".
ARGS ?=
IMAGE ?= curator-manager

CARGO_FLAGS = --locked --profile $(PROFILE)
REPORT = target/smoke-report.txt

.PHONY: help build run test test-unit test-integration smoke fmt fmt-check lint check \
	docker-build docker-run docker-smoke check-report clean

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  %-17s %s\n", $$1, $$2}'

build: ## Compile everything, including the tests
	cargo build $(CARGO_FLAGS) --all-targets

run: ## Run (default: data/events.jsonl and data/policy.json; pass ARGS to change)
	cargo run $(CARGO_FLAGS) -- $(ARGS)

test: ## Run the unit and integration tests
	cargo test $(CARGO_FLAGS)

test-unit: ## Run only the unit tests
	cargo test $(CARGO_FLAGS) --lib --bins

test-integration: ## Run only the integration tests (tests/cli.rs, tests/fuzz.rs)
	cargo test $(CARGO_FLAGS) --test '*'

smoke: ## Run on the supplied data and check which vaults are planned
	@mkdir -p target
	cargo run $(CARGO_FLAGS) > $(REPORT)
	@$(MAKE) --no-print-directory check-report

docker-smoke: ## Run the Docker image on the supplied data and check it
	@mkdir -p target
	docker run --rm $(IMAGE) > $(REPORT)
	@$(MAKE) --no-print-directory check-report

check-report:
	@cat $(REPORT)
	@grep -q '== Vault vault-core \[planned\]' $(REPORT)
	@grep -q '== Vault vault-yield \[planned\]' $(REPORT)
	@grep -q '== Vault vault-legacy \[NOT PLANNED\]' $(REPORT)
	@echo "smoke check passed"

fmt: ## Format the code
	cargo fmt --all

fmt-check: ## Check the formatting
	cargo fmt --all -- --check

lint: ## Run clippy, with warnings as errors
	cargo clippy --locked --all-targets -- -D warnings

check: fmt-check lint test ## Formatting, lints and all tests

docker-build: ## Build the Docker image
	docker build -t $(IMAGE) .

docker-run: ## Run the Docker image on the supplied data
	docker run --rm $(IMAGE)

clean: ## Remove build output
	cargo clean
