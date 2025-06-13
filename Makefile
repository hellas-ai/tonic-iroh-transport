.PHONY: all build check lint test examples clean install-tools help

# Default target
all: check-all lint-all test-all build examples

# Build the main crate
build:
	@echo "Building main crate..."
	cargo build

# Build in release mode
build-release:
	@echo "Building main crate (release)..."
	cargo build --release

# Check code without building
check:
	@echo "Checking main crate..."
	cargo check

# Format code
fmt:
	@echo "Formatting code..."
	cargo fmt

# Check if code is formatted
fmt-check:
	@echo "Checking code formatting..."
	cargo fmt --check

# Lint with clippy
lint:
	@echo "Linting main crate..."
	cargo clippy -- -D warnings

# Run tests
test:
	@echo "Running tests..."
	cargo test

# Run tests with output
test-verbose:
	@echo "Running tests (verbose)..."
	cargo test -- --nocapture

# Build examples
examples:
	@echo "Building chat example..."
	cd examples/chat && cargo build
	@echo "Building echo example..."
	cd examples/echo && cargo build

# Build examples in release mode
examples-release:
	@echo "Building chat example (release)..."
	cd examples/chat && cargo build --release
	@echo "Building echo example (release)..."
	cd examples/echo && cargo build --release

# Test examples
test-examples:
	@echo "Testing chat example..."
	cd examples/chat && cargo test
	@echo "Testing echo example..."
	cd examples/echo && cargo test

# Check examples
check-examples:
	@echo "Checking chat example..."
	cd examples/chat && cargo check
	@echo "Checking echo example..."
	cd examples/echo && cargo check

# Lint examples
lint-examples:
	@echo "Linting chat example..."
	cd examples/chat && cargo clippy -- -D warnings
	@echo "Linting echo example..."
	cd examples/echo && cargo clippy -- -D warnings

# Full check including examples
check-all: check check-examples

# Full lint including examples
lint-all: lint lint-examples

# Full test including examples
test-all: test test-examples

# Clean build artifacts
clean:
	@echo "Cleaning main crate..."
	cargo clean
	@echo "Cleaning chat example..."
	cd examples/chat && cargo clean
	@echo "Cleaning echo example..."
	cd examples/echo && cargo clean

# Install development tools
install-tools:
	@echo "Installing development tools..."
	rustup component add rustfmt clippy

# Run comprehensive checks
ci: fmt-check lint-all test-all build examples

# Development workflow
dev: fmt check lint test

# Show help
help:
	@echo "Available targets:"
	@echo "  all              - Run check, lint, test, build, and examples"
	@echo "  build            - Build the main crate"
	@echo "  build-release    - Build the main crate in release mode"
	@echo "  check            - Check code without building"
	@echo "  check-all        - Check main crate and examples"
	@echo "  check-examples   - Check examples only"
	@echo "  fmt              - Format code"
	@echo "  fmt-check        - Check if code is formatted"
	@echo "  lint             - Lint main crate with clippy"
	@echo "  lint-all         - Lint main crate and examples"
	@echo "  lint-examples    - Lint examples only"
	@echo "  test             - Run tests"
	@echo "  test-verbose     - Run tests with output"
	@echo "  test-all         - Run tests for main crate and examples"
	@echo "  test-examples    - Run tests for examples only"
	@echo "  examples         - Build examples"
	@echo "  examples-release - Build examples in release mode"
	@echo "  clean            - Clean build artifacts"
	@echo "  install-tools    - Install development tools (rustfmt, clippy)"
	@echo "  ci               - Run comprehensive checks (fmt-check, lint-all, test-all, build, examples)"
	@echo "  dev              - Development workflow (fmt, check, lint, test)"
	@echo "  help             - Show this help message"