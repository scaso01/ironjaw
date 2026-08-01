#!/bin/bash
# Quick smoke tests for new Ironjaw features added in the 46-item wave.
# Run after rebuilding and restarting Ironjaw.

BASE="http://localhost:18794"
TOKEN="$(grep '^token' config/ironjaw.toml | head -1 | sed 's/.*= *"//' | sed 's/".*//')"

echo "=== Ironjaw New Feature Smoke Tests ==="
echo ""

# 1. Health check
echo -n "1. Health check: "
health=$(curl -s "$BASE/health" | python -c "import sys,json; print(json.load(sys.stdin)['status'])" 2>/dev/null)
[ "$health" = "ok" ] && echo "PASS" || echo "FAIL ($health)"

# 2. TTS voices endpoint
echo -n "2. TTS voices (21 expected): "
count=$(curl -s "$BASE/health" > /dev/null && echo "need-ws" || echo "0")
echo "SKIP (needs WebSocket — test via Dr. CLAW)"

# 3. mDNS registration (check if service is registered)
echo -n "3. mDNS service: "
# Can't easily test mDNS from CLI, just check the log
echo "SKIP (verify via Android NsdManager)"

# 4. Device presence events
echo -n "4. Device presence: "
echo "SKIP (verify via Dr. CLAW Infrastructure screen)"

# 5. Chatterbox TTS health
echo -n "5. Chatterbox TTS: "
cb_health=$(curl -s http://localhost:8090/health 2>/dev/null | python -c "import sys,json; print(json.load(sys.stdin)['status'])" 2>/dev/null)
[ "$cb_health" = "ok" ] && echo "PASS" || echo "FAIL"

# 6. Chatterbox voice count
echo -n "6. Voice count: "
voices=$(curl -s http://localhost:8090/v1/voices 2>/dev/null | python -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null)
[ "$voices" = "21" ] && echo "PASS (21)" || echo "FAIL ($voices)"

# 7. CC Chronicle insights
echo -n "7. Chronicle insights: "
insights=$(curl -s http://localhost:18793/api/insights 2>/dev/null | python -c "import sys,json; d=json.load(sys.stdin); print('activeProjects' in d)" 2>/dev/null)
[ "$insights" = "True" ] && echo "PASS" || echo "FAIL"

# 8. CC Chronicle avg_length
echo -n "8. Chronicle avg_length: "
avg=$(curl -s http://localhost:18793/api/stats 2>/dev/null | python -c "import sys,json; print(json.load(sys.stdin).get('avg_length_minutes', 'missing'))" 2>/dev/null)
[ "$avg" != "missing" ] && echo "PASS ($avg min)" || echo "FAIL"

echo ""
echo "=== WebSocket tests require Dr. CLAW app ==="
echo "Use the E2E test plan: dr-claw-app/tests/e2e-test-plan.md"
