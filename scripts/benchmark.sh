#!/usr/bin/env bash
# Performance benchmark script for jterm4

set -e

echo "📊 jterm4 Performance Benchmark"
echo "================================"
echo ""

# Always build the measured binary from the current sources.
echo "Building release version..."
nix develop --command bash -c "cargo build --release --locked"

# Binary size
echo "📦 Binary Size:"
ls -lh target/release/jterm4 | awk '{print "   ", $5, $9}'
echo ""

# Headless CLI startup time. This does not claim to measure GTK first-frame time.
echo "⚡ Headless CLI Startup (10 runs):"
total=0
for i in {1..10}; do
    start=$(date +%s%N)
    target/release/jterm4 --version &> /dev/null
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

# Dependency count
echo "📦 Dependencies:"
cargo tree --depth 1 | wc -l | awk '{print "   Direct dependencies:", $1-1}'
echo ""

echo "✅ Benchmark complete!"
