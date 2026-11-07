#!/usr/bin/env bash
# Starts the mock core (SYNTHETIC data) and the gateway, then connects a JSON
# client and a binary client and compares what they receive.
set -euo pipefail
cd "$(dirname "$0")/.."
REL=target/release
$REL/mock_core 5601 5602 100 & CORE=$!
sleep 0.5
$REL/gateway 5600 5601 5602 & GW=$!
trap 'kill $CORE $GW 2>/dev/null || true' EXIT
sleep 1
$REL/demo_client ws://127.0.0.1:5600 "${1:-5}"
