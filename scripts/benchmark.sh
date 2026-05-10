#!/usr/bin/env bash
# Performance benchmark script for jterm4

set -e

echo "📊 jterm4 Performance Benchmark"
echo "================================"
echo ""

# Build release version if needed
if [ ! -f "target/release/jterm4" ]; then
    echo "Building release version..."
    nix develop --command bash -c "cargo build --release"
fi

# Binary size
echo "📦 Binary Size:"
ls -lh target/release/jterm4 | awk '{print "   ", $5, $9}'
echo ""

# Startup time (rough estimate)
echo "⚡ Startup Time (10 runs):"
total=0
for i in {1..10}; do
    start=$(date +%s%N)
    timeout 2 target/release/jterm4 --help &> /dev/null || true
    end=$(date +%s%N)
    elapsed=$((($end - $start) / 1000000))
    total=$(($total + $elapsed))
    echo "   Run $i: ${elapsed}ms"
done
avg=$(($total / 10))
echo "   Average: ${avg}ms"
echo ""

# Memory usage (if jterm4 is running)
echo "💾 Memory Usage:"
if pgrep -x jterm4 > /dev/null; then
    ps aux | grep jterm4 | grep -v grep | awk '{print "   RSS:", $6/1024, "MB"}'
else
    echo "   (jterm4 not running)"
fi
echo ""

# Test suite performance
echo "🧪 Test Suite Performance:"
time_output=$(nix develop --command bash -c "cargo test --lib --test '*' 2>&1 | grep 'test result'")
echo "   $time_output"
echo ""

# Build time
echo "🔨 Incremental Build Time:"
touch src/main.rs
start=$(date +%s)
nix develop --command bash -c "cargo build --release 2>&1" > /dev/null
end=$(date +%s)
elapsed=$(($end - $start))
echo "   ${elapsed}s"
echo ""

# Dependency count
echo "📦 Dependencies:"
cargo tree --depth 1 | wc -l | awk '{print "   Direct dependencies:", $1-1}'
echo ""

echo "✅ Benchmark complete!"
