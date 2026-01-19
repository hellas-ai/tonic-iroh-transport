#!/usr/bin/env bash

# P2P Chat Benchmark Test

# Check for help flag
if [[ "$1" == "--help" || "$1" == "-h" ]]; then
    echo "Usage: $0 [duration] [concurrency]"
    echo "  duration: benchmark duration in seconds (default: 5)"
    echo "  concurrency: number of concurrent connections (default: 10)"
    echo ""
    echo "Examples:"
    echo "  $0           # 5 seconds, 10 concurrent"
    echo "  $0 10        # 10 seconds, 10 concurrent"
    echo "  $0 10 20     # 10 seconds, 20 concurrent"
    exit 0
fi

# Configuration
DURATION=${1:-5}
CONCURRENCY=${2:-10}

echo "=== P2P Chat Benchmark Test ==="
echo "Duration: ${DURATION}s, Concurrency: ${CONCURRENCY}"
echo ""

# Build everything in release mode first
echo -n "Building... "
cargo build --release > /dev/null 2>&1
if [ $? -ne 0 ]; then
    echo "FAILED"
    echo "ERROR: Failed to build release binaries"
    exit 1
fi
echo "OK"

# Clean up any existing processes
pkill -f "p2p-chat" 2>/dev/null || true
while pgrep -f "p2p-chat" > /dev/null; do
    sleep 0.1
done

# Start Node 1 (receiver) in background with minimal logging
echo -n "Starting receiver... "
(RUST_LOG=error time ./target/release/p2p-chat --port 3001 --no-relay > receiver.log 2>&1) &
RECEIVER_PID=$!

# Wait for receiver to start and get Node ID
RECEIVER_ID=""
while [ -z "$RECEIVER_ID" ]; do
    RECEIVER_ID=$(grep "Node ID:" receiver.log 2>/dev/null | head -1 | awk '{print $NF}')
    if [ -z "$RECEIVER_ID" ]; then
        # Check if process is still running
        if ! kill -0 $RECEIVER_PID 2>/dev/null; then
            echo "FAILED"
            echo "ERROR: Receiver process died"
            cat receiver.log
            exit 1
        fi
        sleep 0.1
    fi
done

# Wait for receiver to be ready for connections
while ! grep -q "P2P Chat Node started successfully" receiver.log 2>/dev/null; do
    if ! kill -0 $RECEIVER_PID 2>/dev/null; then
        echo "FAILED"
        echo "ERROR: Receiver process died while waiting for startup"
        cat receiver.log
        exit 1
    fi
    sleep 0.1
done
echo "OK"

# Run benchmark from sender node with minimal logging and capture output
echo -n "Running benchmark... "

RUST_LOG=error ./target/release/p2p-chat --port 3002 --no-relay --connect-to "$RECEIVER_ID" --target-addresses "127.0.0.1:3001" benchmark --duration $DURATION --concurrent $CONCURRENCY > benchmark_results.log 2>&1 &
BENCHMARK_PID=$!

# Wait for benchmark to complete
wait $BENCHMARK_PID
BENCHMARK_EXIT_CODE=$?

if [ $BENCHMARK_EXIT_CODE -ne 0 ]; then
    echo "FAILED"
    echo "ERROR: Benchmark failed with exit code $BENCHMARK_EXIT_CODE"
    cat benchmark_results.log
    exit 1
fi
echo "OK"

# Send shutdown command to receiver
echo -n "Shutting down receiver... "
RUST_LOG=info ./target/release/p2p-chat --port 3003 --no-relay --connect-to "$RECEIVER_ID" --target-addresses "127.0.0.1:3001" shutdown > shutdown.log 2>&1 &
SHUTDOWN_PID=$!

# Wait for shutdown command to complete and check if receiver exits
for i in {1..30}; do
    if ! kill -0 $RECEIVER_PID 2>/dev/null; then
        # Receiver has exited
        wait $SHUTDOWN_PID 2>/dev/null || true
        echo "OK"
        break
    fi

    if ! kill -0 $SHUTDOWN_PID 2>/dev/null; then
        # Shutdown command completed, wait a bit more for receiver
        sleep 1
        if ! kill -0 $RECEIVER_PID 2>/dev/null; then
            echo "OK"
            break
        fi
    fi

    sleep 0.5
done

# If we get here and receiver is still running, something went wrong
if kill -0 $RECEIVER_PID 2>/dev/null; then
    echo "FAILED"
    echo "Shutdown command log:"
    cat shutdown.log
    echo "Receiver still running, check receiver.log:"
    tail -10 receiver.log
    # Clean up
    kill -TERM $RECEIVER_PID 2>/dev/null || true
    wait $RECEIVER_PID 2>/dev/null || true
fi

# Display benchmark results
echo ""
echo "=== Results ==="
if [ -f "benchmark_results.log" ]; then
    grep -E "(Duration|Total messages|Messages/sec|Avg latency)" benchmark_results.log | sed 's/^/  /'
else
    echo "  No benchmark results found"
fi

echo ""
echo "=== Resource Usage ==="
CPU_TIME=$(tail -10 receiver.log | grep -E "(real|user|sys)" | head -1)
if [ -n "$CPU_TIME" ]; then
    echo "  Receiver: $CPU_TIME"
else
    echo "  No timing data available"
fi

# Clean up
pkill -f "p2p-chat" 2>/dev/null || true
echo ""
echo "Benchmark complete."
