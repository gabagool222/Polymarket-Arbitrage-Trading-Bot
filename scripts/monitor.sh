#!/bin/bash
# PolyRustArb Bot Monitor Script
# Usage: ./scripts/monitor.sh

set -e

echo "╔═══════════════════════════════════════════════════════════╗"
echo "║           PolyRustArb Bot - Monitor Dashboard             ║"
echo "╚═══════════════════════════════════════════════════════════╝"
echo ""

# Check if running
if pgrep -x "polyarb" > /dev/null; then
    echo "✅ Bot Status: RUNNING"
else
    echo "❌ Bot Status: STOPPED"
fi

echo ""
echo "=== PNL Summary (Last 24h) ==="
if [ -f "pnl.log" ]; then
    TODAY=$(date +%Y-%m-%d)
    TRADES=$(grep -c "$TODAY" pnl.log 2>/dev/null || echo "0")
    TOTAL_PNL=$(grep "$TODAY" pnl.log 2>/dev/null | jq -s 'map(.net_pnl | tonumber) | add // 0' 2>/dev/null || echo "0")
    echo "Trades Today: $TRADES"
    echo "Net PNL: \$$TOTAL_PNL"
else
    echo "No PNL data yet"
fi

echo ""
echo "=== Rebates Summary ==="
if [ -f "rebates.log" ]; then
    LAST_REBATE=$(tail -1 rebates.log 2>/dev/null | jq -r '.estimated_rebate // "N/A"' 2>/dev/null || echo "N/A")
    VOLUME=$(tail -1 rebates.log 2>/dev/null | jq -r '.maker_volume // "N/A"' 2>/dev/null || echo "N/A")
    echo "Est. Rebates: \$$LAST_REBATE"
    echo "Maker Volume: \$$VOLUME"
else
    echo "No rebates data yet"
fi

echo ""
echo "=== Recent Logs ==="
if [ -f "trades.log" ]; then
    tail -5 trades.log 2>/dev/null || echo "No trades yet"
else
    echo "No trade logs yet"
fi

echo ""
echo "=== Container Logs (if Docker) ==="
docker logs polyarb-bot --tail 10 2>/dev/null || echo "Not running in Docker or container not found"
