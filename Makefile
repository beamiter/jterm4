# jterm4 Makefile
# Convenience wrapper for common development tasks

.PHONY: help build run test check fmt clippy clean install dev watch benchmark debug

help:
	@echo "jterm4 Development Commands"
	@echo "==========================="
	@echo ""
	@echo "Build Commands:"
	@echo "  make build      - Build release version"
	@echo "  make run        - Run in development mode"
	@echo "  make install    - Install to ~/.local/bin"
	@echo ""
	@echo "Quality Commands:"
	@echo "  make test       - Run all tests"
	@echo "  make check      - Check code without building"
	@echo "  make fmt        - Format code"
	@echo "  make clippy     - Lint code"
	@echo ""
	@echo "Development:"
	@echo "  make dev        - Run dev script"
	@echo "  make watch      - Watch for changes and rebuild"
	@echo "  make benchmark  - Run performance benchmarks"
	@echo "  make debug      - Show debug information"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean      - Clean build artifacts"
	@echo ""

build:
	@./scripts/dev.sh build

run:
	@./scripts/dev.sh run

test:
	@./scripts/dev.sh test

check:
	@./scripts/dev.sh check

fmt:
	@./scripts/dev.sh fmt

clippy:
	@./scripts/dev.sh clippy

clean:
	@./scripts/dev.sh clean

install:
	@./scripts/install.sh

dev:
	@./scripts/dev.sh

watch:
	@./scripts/dev.sh watch

benchmark:
	@./scripts/benchmark.sh

debug:
	@./scripts/debug.sh info

# Default target
.DEFAULT_GOAL := help
