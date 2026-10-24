#!/usr/bin/env bash
# cl.sh — end-to-end flow for the concentrated-liquidity (CL) AMM path:
# mint_position -> swap -> collect_fees -> burn_position, asserting real
# numeric outcomes at each step (not just exit codes).
#
# Reuses CL_POOL_CONTRACT_ID from deploy.sh (created via
# factory.create_cl_pool with fee_bps=30, initial_tick=0, tick_spacing=10 —
# see scripts/deploy/pools.sh), the same convention v2.sh uses for the AMM
# pool.
set -Eeuo pipefail

run_cl_flow() {
  CURRENT_FLOW="cl"

  if [[ -z "${CL_POOL_CONTRACT_ID:-}" ]]; then
    die "cl: CL_POOL_CONTRACT_ID not set — did deploy.sh create a CL pool via the factory?"
  fi

  local provider="$SOURCE_PUBLIC_KEY"
  local pool="$CL_POOL_CONTRACT_ID"
  # tick_spacing is 10 for the 30 bps fee tier (see factory::create_cl_pool);
  # a wide range around the pool's initial_tick=0 keeps the position in range
  # for the swap below.
  local lower_tick="${CL_LOWER_TICK:--1000}"
  local upper_tick="${CL_UPPER_TICK:-1000}"
  local amount_a="${CL_AMOUNT_A:-1000000}"
  local amount_b="${CL_AMOUNT_B:-1000000}"
  local swap_amount_in="${CL_SWAP_AMOUNT_IN:-100000}"

  invoke "$TOKEN_A_CONTRACT_ID" mint --to "$provider" --amount "$amount_a" >/dev/null
  invoke "$TOKEN_B_CONTRACT_ID" mint --to "$provider" --amount "$amount_b" >/dev/null
  pass "cl: minted token A/B to test account"

  # ── mint_position ────────────────────────────────────────────────────────
  local mint_output
  mint_output=$(invoke "$pool" mint_position \
    --deadline "$(( $(date +%s) + 300 ))" \
    --provider "$provider" \
    --lower_tick "$lower_tick" \
    --upper_tick "$upper_tick" \
    --amount_a_desired "$amount_a" \
    --amount_b_desired "$amount_b" \
    --min_a 0 \
    --min_b 0)
  pass "cl: mint_position succeeded: $mint_output"

  local liquidity
  liquidity=$(e2e_read_until "cl: get_position liquidity" \
    'invoke "$pool" get_position --provider "$provider" --lower_tick "$lower_tick" --upper_tick "$upper_tick" | field_value liquidity' \
    _is_gt 0)
  assert_gt "cl: get_position reflects nonzero liquidity" "$liquidity" 0

  # ── swap ─────────────────────────────────────────────────────────────────
  local tick_before
  tick_before=$(invoke "$pool" current_tick | parse_i128)

  local deadline
  deadline=$(( $(date +%s) + 300 ))
  local swap_output swap_out
  # zero_for_one=true (sell token A for token B); sqrt_price_limit_x96=0 means
  # "no limit" (see contracts/concentrated_liquidity/src/lib.rs).
  swap_output=$(invoke "$pool" swap \
    --sender "$provider" \
    --zero_for_one true \
    --amount_in "$swap_amount_in" \
    --sqrt_price_limit_x96 0 \
    --min_amount_out 0 \
    --deadline "$deadline")
  swap_out=$(printf '%s\n' "$swap_output" | parse_i128)
  assert_gt "cl: swap returns positive output" "$swap_out" 0

  local tick_after
  tick_after=$(e2e_read_until "cl: current_tick after the swap" \
    'invoke "$pool" current_tick | parse_i128' _is_ne "$tick_before")
  if [[ "$tick_after" == "$tick_before" ]]; then
    die "cl: current_tick did not move after swap (before=$tick_before, after=$tick_after)"
  fi
  pass "cl: current_tick moved after swap: $tick_before -> $tick_after"

  # ── collect_fees ─────────────────────────────────────────────────────────
  local fees_output fee_a fee_b
  fees_output=$(invoke "$pool" collect_fees \
    --provider "$provider" \
    --lower_tick "$lower_tick" \
    --upper_tick "$upper_tick")
  fee_a=$(printf '%s\n' "$fees_output" | grep -Eo -- '-?[0-9]+' | head -n 1)
  fee_b=$(printf '%s\n' "$fees_output" | grep -Eo -- '-?[0-9]+' | sed -n '2p')
  if [[ -z "$fee_a" && -z "$fee_b" ]]; then
    die "cl: collect_fees returned no parsable amounts: $fees_output"
  fi
  if (( ${fee_a:-0} <= 0 && ${fee_b:-0} <= 0 )); then
    die "cl: collect_fees returned zero fees after a swap: $fees_output"
  fi
  pass "cl: collect_fees returned nonzero fees: a=${fee_a:-0} b=${fee_b:-0}"

  # ── burn_position ────────────────────────────────────────────────────────
  local bal_a_before bal_b_before
  bal_a_before=$(invoke "$TOKEN_A_CONTRACT_ID" balance --id "$provider" | parse_i128)
  bal_b_before=$(invoke "$TOKEN_B_CONTRACT_ID" balance --id "$provider" | parse_i128)

  local burn_output
  burn_output=$(invoke "$pool" burn_position \
    --provider "$provider" \
    --lower_tick "$lower_tick" \
    --upper_tick "$upper_tick" \
    --liquidity "$liquidity")
  pass "cl: burn_position succeeded: $burn_output"

  # The burn's payout can trail by a ledger on the node that answers the read.
  local bal_a_after bal_b_after attempt
  for (( attempt = 1; attempt <= E2E_READ_ATTEMPTS; attempt++ )); do
    bal_a_after=$(invoke "$TOKEN_A_CONTRACT_ID" balance --id "$provider" | parse_i128)
    bal_b_after=$(invoke "$TOKEN_B_CONTRACT_ID" balance --id "$provider" | parse_i128)
    if (( bal_a_after > bal_a_before || bal_b_after > bal_b_before )); then
      break
    fi
    if (( attempt < E2E_READ_ATTEMPTS )); then
      printf '[retry] cl: burn_position payout not visible yet; waiting in case the RPC node lags\n' >&2
      sleep "$E2E_READ_WAIT_SECS"
    fi
  done
  if (( bal_a_after <= bal_a_before && bal_b_after <= bal_b_before )); then
    die "cl: burn_position did not return any tokens (a: $bal_a_before -> $bal_a_after, b: $bal_b_before -> $bal_b_after)"
  fi
  pass "cl: burn_position returned tokens to provider (a: $bal_a_before -> $bal_a_after, b: $bal_b_before -> $bal_b_after)"

  assert_eq "cl: position liquidity is zero after full burn" \
    '@cmd:{ invoke "$pool" get_position --provider "$provider" --lower_tick "$lower_tick" --upper_tick "$upper_tick" 2>&1 || true; } | field_value liquidity || echo 0' "0"
}
