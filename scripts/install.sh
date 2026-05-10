#!/usr/bin/env bash
# jterm4 installation script

set -e

echo "🚀 Installing jterm4..."

# Check if nix is available
if ! command -v nix &> /dev/null; then
    echo "❌ Error: nix is required but not found"
    echo "   Please install nix first: https://nixos.org/download.html"
    exit 1
fi

# Build release version
echo "📦 Building release version..."
nix develop --command bash -c "cargo build --release"

# Check if build succeeded
if [ ! -f "target/release/jterm4" ]; then
    echo "❌ Build failed"
    exit 1
fi

# Install binary
INSTALL_DIR="${HOME}/.local/bin"
mkdir -p "${INSTALL_DIR}"

echo "📥 Installing to ${INSTALL_DIR}/jterm4..."
cp target/release/jterm4 "${INSTALL_DIR}/jterm4"
chmod +x "${INSTALL_DIR}/jterm4"

# Create config directory
CONFIG_DIR="${HOME}/.config/jterm4"
mkdir -p "${CONFIG_DIR}"

# Copy example config if no config exists
if [ ! -f "${CONFIG_DIR}/config.toml" ]; then
    if [ -f "config.toml.example" ]; then
        echo "📝 Creating default config at ${CONFIG_DIR}/config.toml..."
        cp config.toml.example "${CONFIG_DIR}/config.toml"
    fi
fi

echo ""
echo "✅ Installation complete!"
echo ""
echo "🎉 jterm4 is now installed at ${INSTALL_DIR}/jterm4"
echo ""
echo "📖 Next steps:"
echo "   1. Make sure ${INSTALL_DIR} is in your PATH"
echo "   2. Run: jterm4"
echo "   3. Edit config: ${CONFIG_DIR}/config.toml"
echo ""
echo "💡 Tips:"
echo "   - Press Ctrl+Shift+K to see all keyboard shortcuts"
echo "   - Use Ctrl+Shift+\\ for split panes"
echo "   - Session state is automatically saved and restored"
echo ""
