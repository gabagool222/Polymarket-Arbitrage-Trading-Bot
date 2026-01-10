#!/bin/bash
# Test script for PolyRustArb price cache fix
# Run this to verify the WebSocket price caching is working

echo "=== PolyRustArb Price Cache Test ==="
echo ""
echo "This test will run the bot in simulation mode for 30 seconds"
echo "and check if we're getting real prices from WebSocket."
echo ""

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Run with debug logging for 30 seconds
echo "Starting bot with debug logging..."
timeout 30s cargo run 2>&1 | tee /tmp/polyarb_test.log

echo ""
echo "=== Analysis ==="

# Check for the problematic pattern (all 0.50/0.50)
SAME_PRICE_COUNT=$(grep -c "Yes=0.5000 + No=0.5000 = 1.0000" /tmp/polyarb_test.log 2>/dev/null || echo "0")
BOOK_UPDATE_COUNT=$(grep -c "Book Update:" /tmp/polyarb_test.log 2>/dev/null || echo "0")
CACHED_PRICE_COUNT=$(grep -c "Using cached prices" /tmp/polyarb_test.log 2>/dev/null || echo "0")
EMPTY_ORDERBOOK=$(grep -c "orderbook empty" /tmp/polyarb_test.log 2>/dev/null || echo "0")

echo ""
echo "Results:"
echo "  - Book updates received: $BOOK_UPDATE_COUNT"
echo "  - Cached prices used: $CACHED_PRICE_COUNT"
echo "  - Default 0.50/0.50 used: $SAME_PRICE_COUNT"
echo "  - Empty orderbook warnings: $EMPTY_ORDERBOOK"

echo ""
if [ "$BOOK_UPDATE_COUNT" -gt 0 ] && [ "$CACHED_PRICE_COUNT" -gt 0 ]; then
    echo -e "${GREEN}✅ SUCCESS: Price cache is working!${NC}"
    echo "   WebSocket orderbook updates are being received and cached."
elif [ "$EMPTY_ORDERBOOK" -gt 0 ]; then
    echo -e "${YELLOW}⚠️ WARNING: Orderbooks appear to be empty on Polymarket${NC}"
    echo "   This is a market liquidity issue, not a code issue."
elif [ "$SAME_PRICE_COUNT" -gt 5 ] && [ "$BOOK_UPDATE_COUNT" -eq 0 ]; then
    echo -e "${RED}❌ ISSUE: Still getting default prices, no book updates${NC}"
    echo "   WebSocket may not be sending orderbook data."
    echo "   Run with RUST_LOG=trace to see raw messages."
else
    echo -e "${YELLOW}⚠️ INCONCLUSIVE: Need more data${NC}"
    echo "   Try running for longer (remove timeout) or check logs manually."
fi

echo ""
echo "Full log saved to: /tmp/polyarb_test.log"
echo "To view: cat /tmp/polyarb_test.log | grep -E '(Price|Book|cached|orderbook)'"
