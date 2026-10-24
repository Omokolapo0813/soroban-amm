#!/usr/bin/env bash
# common.test.sh — offline tests for the assertion helpers in common.sh.
# No network: reads are stubbed. Run: bash scripts/e2e/common.test.sh
set -Eeuo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/common.sh"

E2E_READ_WAIT_SECS=0
COUNTER="$(mktemp)"
trap 'rm -f "$COUNTER"' EXIT
FAILED=0

# calls_until_ok N OK_VALUE STALE_VALUE — prints STALE_VALUE for the first N-1
# calls and OK_VALUE afterwards. State lives in a file because a deferred read
# runs in a subshell.
calls_until_ok() {
  local n=$(( $(cat "$COUNTER") + 1 ))
  echo "$n" > "$COUNTER"
  if (( n < $1 )); then printf '%s\n' "$3"; else printf '%s\n' "$2"; fi
}

# a "transaction": reports submission on stderr like the stellar CLI
send_tx() {
  echo "$(( $(cat "$COUNTER") + 1 ))" > "$COUNTER"
  echo "Transaction submitted successfully!" >&2
  printf '0\n'
}

expect_ok() {
  local name="$1"; shift
  echo 0 > "$COUNTER"
  if ( "$@" ) >/dev/null 2>&1; then echo "ok   - $name"; else echo "FAIL - $name"; FAILED=1; fi
}

expect_die() {
  local name="$1"; shift
  echo 0 > "$COUNTER"
  if ( "$@" ) >/dev/null 2>&1; then echo "FAIL - $name (should have failed)"; FAILED=1; else echo "ok   - $name"; fi
}

expect_calls() {
  local name="$1" want="$2"
  if [[ "$(cat "$COUNTER")" == "$want" ]]; then echo "ok   - $name ($want call(s))"; else echo "FAIL - $name: $(cat "$COUNTER") call(s), wanted $want"; FAILED=1; fi
}

# literals behave exactly as before
expect_ok  "literal eq passes"           assert_eq "t" "5" "5"
expect_die "literal eq mismatch dies"    assert_eq "t" "5" "6"
expect_ok  "literal gt passes"           assert_gt "t" "5" 4
expect_die "literal gt dies"             assert_gt "t" "5" 5
expect_die "non-numeric gt dies"         assert_gt "t" "abc" 1
expect_ok  "literal between passes"      assert_between "t" "5" 1 10
expect_ok  "literal contains passes"     assert_contains "t" "hello world" "wor"
expect_ok  "literal not_contains passes" assert_not_contains "t" "hello" "xyz"
expect_ok  "lte_abs passes"              assert_lte_abs "t" "-3" 3

# a stale read settles on a later attempt
expect_ok  "eq settles"        assert_eq "t" '@cmd:calls_until_ok 3 7 0' "7"
expect_ok  "gt settles"        assert_gt "t" '@cmd:calls_until_ok 3 7 0' 5
expect_ok  "between settles"   assert_between "t" '@cmd:calls_until_ok 3 7 0' 5 9
expect_ok  "contains settles"  assert_contains "t" '@cmd:calls_until_ok 3 revoked pending' "revoked"
expect_ok  "not_contains settles" assert_not_contains "t" '@cmd:calls_until_ok 3 gone present' "present"
expect_ok  "lte_abs settles"   assert_lte_abs "t" '@cmd:calls_until_ok 3 1 500' 5
expect_ok  "settled wrapper"   assert_eq_settled "t" "7" calls_until_ok 3 7 0
expect_ok  "contains wrapper"  assert_contains_settled "t" "ok" calls_until_ok 3 ok no

# a fresh read is not repeated
echo 0 > "$COUNTER"; assert_eq "t" '@cmd:calls_until_ok 1 7 0' "7" >/dev/null
expect_calls "fresh read costs one call" 1

# a read that never settles dies after exactly E2E_READ_ATTEMPTS reads
expect_die "stale forever dies" assert_eq "t" '@cmd:calls_until_ok 999 7 0' "7"
echo 0 > "$COUNTER"; ( assert_eq "t" '@cmd:calls_until_ok 999 7 0' "7" ) >/dev/null 2>&1 || true
expect_calls "gives up after the attempt limit" "$E2E_READ_ATTEMPTS"

# a deferred transaction is never repeated
echo 0 > "$COUNTER"; ( assert_eq "t" '@cmd:send_tx' "1" ) >/dev/null 2>&1 || true
expect_calls "a write is not re-run on mismatch" 1

# the deferred code sees the caller's local variables
local_scope() { local who="alice"; assert_eq "t" '@cmd:printf %s "$who"' "alice"; }
expect_ok "deferred code sees caller locals" local_scope

# a failing deferred command counts as an empty read, not a crash
expect_die "failing command dies via the assertion" assert_eq "t" '@cmd:false' "x"

exit "$FAILED"
