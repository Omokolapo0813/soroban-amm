#!/usr/bin/env bash
# twap.sh — end-to-end flow for the oracle consumers (issue #985):
# save_snapshot through the normal simulate-then-submit path, wait out a
# 60-second window, then read a TWAP (and a TWAL) over it.
#
# Every client (this CLI, the SDKs, examples/python/twap_client.py) simulates
# a transaction against the latest closed ledger and applies it in a later
# one. When snapshots were keyed by the ledger timestamp, the key written at
# apply time was never the key simulation had put in the footprint, so every
# save_snapshot trapped with "trying to access contract data key outside of
# the footprint". Unit tests cannot see that — Env::default() does not
# enforce footprints — so this flow is what guards it.
#
# Uses the shared AMM pool and the consumers from scripts/deploy.sh. Both
# consumers' keeper is the deploy admin, which must be this flow's source
# account.
set -Eeuo pipefail

run_twap_flow() {
  CURRENT_FLOW="twap"

  if [[ -z "${TWAP_CONSUMER_CONTRACT_ID:-}" ]]; then
    die "twap: TWAP_CONSUMER_CONTRACT_ID not set — deploy twap_consumer first"
  fi
  local window="${TWAP_WINDOW_SECS:-60}"
  local amount=1000000
  assert_eq "twap: consumer keeper is this account" \
    '@cmd:invoke "$TWAP_CONSUMER_CONTRACT_ID" get_keeper | extract_account_id' "$SOURCE_PUBLIC_KEY"

  # The pool needs liquidity for its price accumulators to move.
  invoke "$TOKEN_A_CONTRACT_ID" mint --to "$SOURCE_PUBLIC_KEY" --amount "$(( amount * 2 ))" >/dev/null
  invoke "$TOKEN_B_CONTRACT_ID" mint --to "$SOURCE_PUBLIC_KEY" --amount "$amount" >/dev/null
  invoke "$AMM_CONTRACT_ID" add_liquidity \
    --provider "$SOURCE_PUBLIC_KEY" \
    --amount_a "$amount" \
    --amount_b "$amount" \
    --min_shares 0 \
    --deadline "$(( $(date +%s) + 300 ))" >/dev/null
  pass "twap: seeded liquidity on the shared AMM pool"

  # ── save snapshots (the calls that used to trap on apply) ────────────────
  local count_before count_after
  count_before=$(invoke "$TWAP_CONSUMER_CONTRACT_ID" get_snapshot_count --pool "$AMM_CONTRACT_ID" | parse_i128)
  invoke "$TWAP_CONSUMER_CONTRACT_ID" save_snapshot --pool "$AMM_CONTRACT_ID" >/dev/null
  # A same-ledger save overwrites, so the count may stay put; it must not
  # drop, and must be at least 1.
  count_after=$(e2e_read_until "twap: snapshot count after save" \
    'invoke "$TWAP_CONSUMER_CONTRACT_ID" get_snapshot_count --pool "$AMM_CONTRACT_ID" | parse_i128' \
    _is_ge "$(( count_before > 1 ? count_before : 1 ))")
  if (( count_after < 1 || count_after < count_before )); then
    die "twap: save_snapshot applied but snapshot count went from $count_before to $count_after"
  fi
  pass "twap: save_snapshot applied on-chain (snapshots: $count_after)"

  local twal=""
  if [[ -n "${TWAL_CONSUMER_CONTRACT_ID:-}" ]]; then
    twal="$TWAL_CONSUMER_CONTRACT_ID"
    invoke "$twal" save_snapshot --pool "$AMM_CONTRACT_ID" >/dev/null
    local twal_count
    twal_count=$(e2e_read_until "twap: TWAL snapshot count" \
      'invoke "$twal" get_snapshot_count --pool "$AMM_CONTRACT_ID" | parse_i128' _is_gt 0)
    if (( twal_count < 1 )); then
      die "twap: TWAL save_snapshot applied but no snapshot is stored"
    fi
    pass "twap: TWAL save_snapshot applied on-chain (snapshots: $twal_count)"
  fi

  # ── wait out the window, then move the pool so its clock advances ────────
  pass "twap: sleeping $(( window + 5 ))s so the snapshot predates the ${window}s window"
  sleep "$(( window + 5 ))"
  invoke "$AMM_CONTRACT_ID" swap \
    --trader "$SOURCE_PUBLIC_KEY" \
    --token_in "$TOKEN_A_CONTRACT_ID" \
    --amount_in 10000 \
    --min_out 0 \
    --deadline "$(( $(date +%s) + 300 ))" >/dev/null
  pass "twap: swapped to advance the pool's accumulators"

  # ── read over the window ──────────────────────────────────────────────────
  local twap_price
  twap_price=$(e2e_read_until "twap: get_twap_price" \
    'invoke "$TWAP_CONSUMER_CONTRACT_ID" get_twap_price --pool "$AMM_CONTRACT_ID" --window_seconds "$window" | parse_i128' \
    _is_gt 0)
  if [[ -z "$twap_price" ]] || (( twap_price <= 0 )); then
    die "twap: get_twap_price over ${window}s returned '${twap_price}', expected a positive price"
  fi
  pass "twap: get_twap_price over ${window}s = $twap_price"

  if [[ -n "$twal" ]]; then
    local twal_value
    twal_value=$(e2e_read_until "twap: get_twal_liquidity" \
      'invoke "$twal" get_twal_liquidity --pool "$AMM_CONTRACT_ID" --window_seconds "$window" | parse_i128' \
      _is_gt 0)
    if [[ -z "$twal_value" ]] || (( twal_value <= 0 )); then
      die "twap: get_twal_liquidity over ${window}s returned '${twal_value}', expected positive liquidity"
    fi
    pass "twap: get_twal_liquidity over ${window}s = $twal_value"
  fi
}

extract_account_id() {
  grep -Eo 'G[A-Z0-9]{55}' | tail -n 1
}
