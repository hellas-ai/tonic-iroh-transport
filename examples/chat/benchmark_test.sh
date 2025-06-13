#!/bin/bash

# P2P Chat Benchmark Test

echo "=== P2P Chat Benchmark Test ==="

# Clean up any existing processes
pkill -f "p2p-chat" 2>/dev/null || true
sleep 1

# Start Node 1 (receiver) in background
echo "Starting receiver node on port 3001 (no relay)..."
cargo run -- --port 3001 --no-relay --verbose > receiver.log 2>&1 &
RECEIVER_PID=$!
echo "Receiver PID: $RECEIVER_PID"

# Wait for receiver to start
sleep 3

# Get receiver's Node ID
RECEIVER_ID=$(grep "Node ID:" receiver.log | head -1 | awk '{print $NF}')
if [ -z "$RECEIVER_ID" ]; then
    echo "ERROR: Could not get receiver Node ID"
    kill $RECEIVER_PID 2>/dev/null
    exit 1
fi

echo "Receiver Node ID: $RECEIVER_ID"

# Run benchmark from sender node
echo "Starting benchmark: 5 seconds, 10 concurrent connections..."
echo "Command: cargo run -- --port 3002 --connect-to $RECEIVER_ID --target-addresses 127.0.0.1:3001 benchmark --duration 5 --concurrent 10"

cargo run -- --port 3002 --no-relay --verbose --connect-to "$RECEIVER_ID" --target-addresses "127.0.0.1:3001" benchmark --duration 5 --concurrent 10

echo ""
echo "=== Receiver Log (last 30 lines) ==="
tail -30 receiver.log

# Clean up
echo ""
echo "Cleaning up..."
kill $RECEIVER_PID 2>/dev/null || true
pkill -f "p2p-chat" 2>/dev/null || true

echo "Benchmark complete."