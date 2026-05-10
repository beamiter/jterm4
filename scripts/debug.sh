#!/usr/bin/env bash
# Debug helper script for jterm4

set -e

CMD="${1:-info}"

case "$CMD" in
    info)
        echo "🔍 jterm4 Debug Information"
        echo "==========================="
        echo ""
        echo "📂 Paths:"
        echo "   Config: ~/.config/jterm4/config.toml"
        echo "   State: ~/.config/jterm4/tabs.state"
        echo "   Binary: $(which jterm4 2>/dev/null || echo 'Not in PATH')"
        echo ""
        echo "📊 State File:"
        if [ -f ~/.config/jterm4/tabs.state ]; then
            echo "   Size: $(wc -c < ~/.config/jterm4/tabs.state) bytes"
            echo "   Lines: $(wc -l < ~/.config/jterm4/tabs.state)"
            echo "   Content:"
            cat ~/.config/jterm4/tabs.state | head -10 | sed 's/^/      /'
        else
            echo "   (No state file)"
        fi
        echo ""
        echo "⚙️  Config File:"
        if [ -f ~/.config/jterm4/config.toml ]; then
            echo "   Exists: Yes"
            echo "   Mode: $(grep '^terminal_mode' ~/.config/jterm4/config.toml || echo 'default')"
            echo "   Theme: $(grep '^theme' ~/.config/jterm4/config.toml || echo 'default')"
        else
            echo "   (No config file - using defaults)"
        fi
        echo ""
        echo "🔧 Running Processes:"
        ps aux | grep jterm4 | grep -v grep || echo "   (No jterm4 processes)"
        ;;

    logs)
        echo "📜 Running jterm4 with debug logs..."
        JTERM4_LOG=debug target/release/jterm4
        ;;

    trace)
        echo "🔬 Running jterm4 with trace logs..."
        JTERM4_LOG=trace target/release/jterm4
        ;;

    state)
        echo "📊 Current State File:"
        if [ -f ~/.config/jterm4/tabs.state ]; then
            cat ~/.config/jterm4/tabs.state
        else
            echo "(No state file)"
        fi
        ;;

    clean-state)
        echo "🧹 Cleaning state file..."
        if [ -f ~/.config/jterm4/tabs.state ]; then
            rm ~/.config/jterm4/tabs.state
            echo "✅ State file removed"
        else
            echo "No state file to remove"
        fi
        ;;

    reset-config)
        echo "🔄 Resetting config to defaults..."
        if [ -f config.toml.example ]; then
            cp config.toml.example ~/.config/jterm4/config.toml
            echo "✅ Config reset to defaults"
        else
            echo "❌ config.toml.example not found"
        fi
        ;;

    valgrind)
        echo "🔬 Running with valgrind..."
        valgrind --leak-check=full --show-leak-kinds=all target/release/jterm4
        ;;

    strace)
        echo "🔍 Running with strace..."
        strace -o /tmp/jterm4-strace.log target/release/jterm4
        echo "Trace saved to /tmp/jterm4-strace.log"
        ;;

    *)
        echo "Usage: $0 {info|logs|trace|state|clean-state|reset-config|valgrind|strace}"
        echo ""
        echo "Commands:"
        echo "  info         - Show debug information"
        echo "  logs         - Run with debug logs"
        echo "  trace        - Run with trace logs"
        echo "  state        - Show current state file"
        echo "  clean-state  - Remove state file"
        echo "  reset-config - Reset config to defaults"
        echo "  valgrind     - Run with valgrind"
        echo "  strace       - Run with strace"
        exit 1
        ;;
esac
