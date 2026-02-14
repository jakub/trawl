#!/bin/bash
# fleet DSL chaos smoke test
# target: ./data/**/*.parquet (~4.7M macOS system log rows)

set -euo pipefail

FLEET="./target/release/fleet"
DATA="./data/**/*.parquet"
INSECURE="--insecure"
FAILED=0
PASSED=0

# color output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # no color

run_query() {
    local name="$1"
    local query="$2"
    local expect_success="${3:-true}"

    echo -ne "${YELLOW}TEST:${NC} $name ... "

    if output=$("$FLEET" $INSECURE --data="$DATA" -- "$query" 2>&1); then
        if [ "$expect_success" = "true" ]; then
            echo -e "${GREEN}PASS${NC}"
            PASSED=$((PASSED + 1))
            return 0
        else
            echo -e "${RED}FAIL${NC} (expected failure but got success)"
            echo "  query: $query"
            echo "  output: ${output:0:500}..."  # truncate to 500 chars
            FAILED=$((FAILED + 1))
            return 1
        fi
    else
        exit_code=$?
        if [ "$expect_success" = "false" ]; then
            echo -e "${GREEN}PASS${NC} (expected failure)"
            PASSED=$((PASSED + 1))
            return 0
        else
            echo -e "${RED}FAIL${NC} (exit code $exit_code)"
            echo "  query: $query"
            echo "  error: $output"
            FAILED=$((FAILED + 1))
            return 1
        fi
    fi
}

echo "=== fleet DSL chaos smoke test ==="
echo "data source: $DATA"
echo ""

# === BASIC SANITY ===
echo "--- basic sanity ---"
run_query "empty query" "* | limit 10"
run_query "simple field filter" "service:Dock"
run_query "simple stats" "service:Dock | stats count()"
run_query "simple sort" "service:Dock | sort -timestamp | limit 10"

# === OR OPERATOR ===
echo ""
echo "--- OR operator ---"
run_query "simple OR" "service:Dock OR service:apsd"
run_query "OR with stats" "service:Dock OR service:apsd | stats count()"
run_query "OR with multiple terms per group" "service:Dock level:error OR service:apsd level:warn"
run_query "triple OR" "service:Dock OR service:apsd OR service:locationd | stats count()"
run_query "OR with negation" "service:Dock -debug OR service:apsd -trace | limit 100"
run_query "OR mixed case" "service:Dock or service:apsd | limit 100"
run_query "OR with quoted search" '"connection" OR "timeout" | limit 100'
run_query "complex OR pipeline" 'service:Dock OR service:apsd | stats count() by service | where count > 10 | sort -count'

# === QUOTED VALUES ===
echo ""
echo "--- quoted values ---"
run_query "quoted field value" 'service:"Dock"'
run_query "quoted value with spaces" 'service:"Activity Monitor" | limit 5'
run_query "quoted search phrase" '"memory pressure"'
run_query "quoted regex-like value" 'message:"/error.*/"'  # should treat as literal, not regex

# === REGEX & GLOB ===
echo ""
echo "--- regex & glob patterns ---"
run_query "glob pattern" "service:*d | limit 100"
run_query "glob with stats" "service:*d | stats count() by service"
run_query "regex pattern" "message:/error/ | limit 100"
# Note: regex flags like /i are NOT implemented — /ERROR/i matches literal "ERROR/i" (returns 0)
run_query "glob question mark" "service:?????d | limit 100"  # 5 chars + d

# === FIELD FILTERS WITH OPERATORS ===
echo ""
echo "--- field filter operators ---"
run_query "not equal" "level:!=info | limit 5"
run_query "comma list" "service:Dock,apsd,locationd | stats count() by service"
# Note: numeric comparison tests skipped - no numeric fields in macOS syslog data

# === NEGATED SEARCHES ===
echo ""
echo "--- negated searches ---"
run_query "simple negation" "-debug | limit 5"
run_query "negation with field" "-debug service:Dock | limit 5"
run_query "NOT keyword" "NOT level:debug | limit 5"
run_query "multiple negations" "-debug -trace -verbose | limit 5"
run_query "negation in OR group" "service:Dock -error OR service:apsd -warn"

# === TIME FILTERS ===
echo ""
echo "--- time filters ---"
run_query "last hour" "last:1h | stats count()"
run_query "last day (probably empty)" "last:24h | stats count()"
run_query "last minute" "last:1m | stats count()"
run_query "time filter with OR" "last:1h service:Dock OR service:apsd | stats count()"
run_query "time filter late in query" "service:Dock last:1h | stats count()"

# === STATS VARIATIONS ===
echo ""
echo "--- stats aggregations ---"
run_query "count" "* | stats count()"
run_query "count by field" "* | stats count() by service"
run_query "multiple group-by" "* | stats count() by service, level"
run_query "multiple aggs" "* | stats count() by service, level | limit 10"
run_query "stats with where" "* | stats count() by service | where count > 100"
run_query "stats with sort" "* | stats count() by service | sort -count"

# === DEEP PIPELINES ===
echo ""
echo "--- deep pipelines ---"
run_query "5-stage pipeline" "service:Dock | stats count() by level | where count > 1 | sort -count | limit 3"
run_query "filters -> stats -> where -> sort -> limit" "service:*d level:!=debug | stats count() by service | where count > 10 | sort -count | limit 5"
run_query "OR in deep pipeline" "service:Dock OR service:apsd | stats count() by service, level | where count > 5 | sort service, -count"

# === EDGE CASES ===
echo ""
echo "--- edge cases ---"
run_query "empty result set" "service:NONEXISTENT_SERVICE_12345 | stats count()"
run_query "field that doesn't exist" "nosuchfield:value | limit 1" false  # expect 400
run_query "only whitespace filter" "service: | limit 1" false  # malformed
run_query "trailing pipe" "service:Dock |" false  # incomplete
run_query "double OR" "service:Dock OR OR service:apsd" # should handle gracefully
run_query "leading OR" "OR service:Dock" # should handle gracefully
run_query "only OR" "OR" # should parse as text search "OR"
run_query "unicode in search" "message:café" false  # may not exist but should parse
run_query "very long field name" "a$(printf 'b%.0s' {1..100}):value" false  # probably doesn't exist

# === PATHOLOGICAL INPUTS ===
echo ""
echo "--- pathological inputs ---"
run_query "deeply nested parens (if supported)" "((service:Dock))" false  # probably not supported
run_query "unclosed quote" '"unterminated' false
run_query "backslash escapes" 'message:"test\"quote"' # should work if escaping supported
run_query "injection attempt (safe)" "service:'; DROP TABLE logs; --" # should be safely parameterized
run_query "glob everywhere" "**********" # should parse as text search
run_query "regex with unbalanced parens" "message:/test(/" false
# numeric filter tests skipped - no numeric fields in macOS syslog data
run_query "empty comma list" "service:," false  # malformed
run_query "comma without values" "service:a,,b" # should handle gracefully

# === TIMECHART (if implemented) ===
echo ""
echo "--- timechart (may not be implemented yet) ---"
run_query "basic timechart" "* | timechart count()" false  # may not exist yet
run_query "timechart with span" "* | timechart span=5m count()" false

# === SORT VARIATIONS ===
echo ""
echo "--- sort variations ---"
run_query "sort asc" "* | limit 100 | sort timestamp | limit 5"
run_query "sort" "* | limit 100 | sort -timestamp | limit 5"
run_query "sort by multiple fields" "* | stats count() by service, level | sort service, -count"
run_query "sort without direction" "* | limit 100 | sort timestamp | limit 5"

# === WHERE VARIATIONS ===
echo ""
echo "--- where clause variations ---"
run_query "where with AND" "* | stats count() by service | where count > 10 AND count < 100" false  # AND may not be supported
run_query "where with OR" "* | stats count() by service | where count > 100 OR count < 5" false  # OR in where may not be supported
run_query "where with string comparison" '* | stats count() by service | where service = "Dock"' false  # may need different syntax
run_query "where with != on aggregation" "* | stats count() by service | where count != 1"

# === COMBO CHAOS ===
echo ""
echo "--- maximum chaos combos ---"
run_query "everything at once" 'last:2h service:*d level:!=debug "error" -trace OR service:Dock "timeout" | stats count() by service, level | where count > 5 | sort -count, service | limit 10'
run_query "OR with deep pipeline each group" '(service:Dock level:error last:1h) OR (service:apsd level:warn) | stats count() by service | where count > 0 | sort -count'
run_query "negation spam" '-a -b -c -d -e -f | limit 1'  # lots of negations
run_query "glob + regex + or + stats" 'service:*d OR message:/error/ | stats count() by service | where count > 10'

echo ""
echo "=== SUMMARY ==="
echo -e "${GREEN}PASSED: $PASSED${NC}"
echo -e "${RED}FAILED: $FAILED${NC}"

if [ "$FAILED" -gt 0 ]; then
    exit 1
fi

exit 0
