.PHONY: help install lint format test build pre-commit clean integration-test

help: ## Show this help message
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

install: ## Fetch Rust dependencies
	cargo fetch

lint: ## Run clippy linter
	cargo clippy --all-targets -- -D warnings

format: ## Run rustfmt
	cargo fmt

test: ## Run unit tests (no external services needed)
	cargo test

build: ## Build Rust project
	cargo build

pre-commit: ## Run pre-commit hooks on all files
	pre-commit run --all-files

clean: ## Remove Rust build artifacts
	cargo clean

integration-test: ## Run integration tests (starts OGx, runs tests, tears down)
	./tests/integration/run.sh
