.PHONY: all build check lint test examples clean install-tools help fmt fmt-examples fmt-all fmt-check fmt-check-examples fmt-check-all lint-proto

# Default target
all: fmt-all check-all lint-all test-all doc-check-all build examples

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
	cargo fmt --all

# Format examples
fmt-examples:
	@echo "Formatting chat example..."
	cd examples/chat && cargo fmt --all
	@echo "Formatting echo example..."
	cd examples/echo && cargo fmt --all

# Format all code including examples
fmt-all: fmt fmt-examples

# Check if code is formatted
fmt-check:
	@echo "Checking code formatting..."
	cargo fmt --all --check

# Check if examples are formatted
fmt-check-examples:
	@echo "Checking chat example formatting..."
	cd examples/chat && cargo fmt --all --check
	@echo "Checking echo example formatting..."
	cd examples/echo && cargo fmt --all --check

# Check all code formatting including examples
fmt-check-all: fmt-check fmt-check-examples

# Lint with clippy
lint:
	@echo "Linting main crate..."
	cargo clippy -- -D warnings

# Generate documentation
doc:
	@echo "Generating docs for main crate..."
	cargo doc --no-deps

# Generate documentation with dependencies
doc-full:
	@echo "Generating docs for main crate with dependencies..."
	cargo doc

# Check documentation builds without warnings
doc-check:
	@echo "Checking docs for main crate..."
	cargo doc --no-deps --document-private-items

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

# Lint proto files with buf
lint-proto:
	@echo "Linting proto files..."
	buf lint examples/chat/proto
	buf lint examples/echo/proto

# Generate docs for examples
doc-examples:
	@echo "Generating docs for chat example..."
	cd examples/chat && cargo doc --no-deps
	@echo "Generating docs for echo example..."
	cd examples/echo && cargo doc --no-deps

# Check docs for examples
doc-check-examples:
	@echo "Checking docs for chat example..."
	cd examples/chat && cargo doc --no-deps --document-private-items
	@echo "Checking docs for echo example..."
	cd examples/echo && cargo doc --no-deps --document-private-items

# Full check including examples
check-all: check check-examples

# Full lint including examples and protos
lint-all: lint lint-examples lint-proto

# Full test including examples
test-all: test test-examples

# Full doc generation including examples
doc-all: doc doc-examples

# Full doc check including examples
doc-check-all: doc-check doc-check-examples

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
ci: fmt-check-all lint-all test-all doc-check-all build examples

# Development workflow
dev: fmt-all check lint test

# Show help
help:
	@echo "Available targets:"
	@echo "  all              - Run format, check, lint, test, build, and examples"
	@echo "  build            - Build the main crate"
	@echo "  build-release    - Build the main crate in release mode"
	@echo "  check            - Check code without building"
	@echo "  check-all        - Check main crate and examples"
	@echo "  check-examples   - Check examples only"
	@echo "  fmt              - Format main crate code"
	@echo "  fmt-examples     - Format examples code"
	@echo "  fmt-all          - Format main crate and examples"
	@echo "  fmt-check        - Check if main crate code is formatted"
	@echo "  fmt-check-examples - Check if examples code is formatted"
	@echo "  fmt-check-all    - Check if all code is formatted"
	@echo "  lint             - Lint main crate with clippy"
	@echo "  lint-all         - Lint main crate, examples, and protos"
	@echo "  lint-examples    - Lint examples only"
	@echo "  lint-proto       - Lint proto files with buf"
	@echo "  test             - Run tests"
	@echo "  test-verbose     - Run tests with output"
	@echo "  test-all         - Run tests for main crate and examples"
	@echo "  test-examples    - Run tests for examples only"
	@echo "  doc              - Generate docs for main crate"
	@echo "  doc-full         - Generate docs for main crate with dependencies"
	@echo "  doc-check        - Check docs build for main crate"
	@echo "  doc-all          - Generate docs for main crate and examples"
	@echo "  doc-check-all    - Check docs build for main crate and examples"
	@echo "  doc-examples     - Generate docs for examples only"
	@echo "  doc-check-examples - Check docs build for examples only"
	@echo "  examples         - Build examples"
	@echo "  examples-release - Build examples in release mode"
	@echo "  clean            - Clean build artifacts"
	@echo "  install-tools    - Install development tools (rustfmt, clippy)"
	@echo "  ci               - Run comprehensive checks (fmt-check, lint-all, test-all, doc-check-all, build, examples)"
	@echo "  dev              - Development workflow (fmt, check, lint, test)"
	@echo "  help             - Show this help message"
