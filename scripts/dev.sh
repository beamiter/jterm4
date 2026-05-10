#!/usr/bin/env bash
# Development convenience script

set -e

CMD="${1:-run}"

case "$CMD" in
    run)
        echo "🚀 Running jterm4 in development mode..."
        nix develop --command bash -c "cargo run"
        ;;

    build)
        echo "🔨 Building jterm4..."
        nix develop --command bash -c "cargo build --release"
        ;;

    test)
        echo "🧪 Running tests..."
        nix develop --command bash -c "cargo test --lib --test '*'"
        ;;

    check)
        echo "✅ Checking code..."
        nix develop --command bash -c "cargo check"
        ;;

    fmt)
        echo "🎨 Formatting code..."
        nix develop --command bash -c "cargo fmt"
        ;;

    clippy)
        echo "📎 Running clippy..."
        nix develop --command bash -c "cargo clippy -- -D warnings"
        ;;

    clean)
        echo "🧹 Cleaning build artifacts..."
        cargo clean
        ;;

    watch)
        echo "👀 Watching for changes..."
        if ! command -v cargo-watch &> /dev/null; then
            echo "Installing cargo-watch..."
            cargo install cargo-watch
        fi
        nix develop --command bash -c "cargo watch -x run"
        ;;

    *)
        echo "Usage: $0 {run|build|test|check|fmt|clippy|clean|watch}"
        echo ""
        echo "Commands:"
        echo "  run     - Run jterm4 in development mode"
        echo "  build   - Build release version"
        echo "  test    - Run all tests"
        echo "  check   - Check code without building"
        echo "  fmt     - Format code"
        echo "  clippy  - Lint code"
        echo "  clean   - Clean build artifacts"
        echo "  watch   - Watch for changes and rebuild"
        exit 1
        ;;
esac
