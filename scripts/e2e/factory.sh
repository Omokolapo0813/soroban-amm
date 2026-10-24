#!/usr/bin/env bash
# factory.sh — end-to-end flow for the factory-driven pool creation path,
# which is how pools are meant to be created in production. Deploys a fresh
# factory (not shared with the v2/other flows) so pause/unpause and
# fee-tier changes here cannot affect other flows' pools.
set -Eeuo pipefail

run_factory_flow() {
  CURRENT_FLOW="factory"

  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then
    die "factory: FACTORY_CONTRACT_ID not set — did deploy.sh run the factory step?"
  fi
  if [[ -z "${AMM_WASM_HASH:-}" || -z "${TOKEN_WASM_HASH:-}" ]]; then
    die "factory: AMM_WASM_HASH / TOKEN_WASM_HASH not set — factory needs registered pool WASM hashes"
  fi

  local admin="$SOURCE_PUBLIC_KEY"
  local ta="$TOKEN_A_CONTRACT_ID"
  local tb="$TOKEN_B_CONTRACT_ID"

  # ── create_pool_with_fee_bps ────────────────────────────────────────────
  local out pool_addr
  out=$(invoke "$FACTORY_CONTRACT_ID" create_pool_with_fee_bps \
    --caller "$admin" \
    --token_a "$ta" \
    --token_b "$tb" \
    --fee_bps 25) || {
    # Idempotent: a prior run may have already created this pair's pool.
    pool_addr=$(invoke "$FACTORY_CONTRACT_ID" get_pool --token_a "$ta" --token_b "$tb" | extract_contract_id || true)
    if [[ -z "$pool_addr" ]]; then
      die "factory: create_pool_with_fee_bps failed and no existing pool found: $out"
    fi
  }
  if [[ -z "${pool_addr:-}" ]]; then
    pool_addr=$(printf '%s\n' "$out" | extract_contract_id)
  fi
  if [[ -z "$pool_addr" ]]; then
    die "factory: could not parse pool address from create_pool_with_fee_bps output: $out"
  fi
  pass "factory: created pool via factory: $pool_addr"

  # ── get_pool resolves the pair to the new address ──────────────────────
  assert_eq "factory: get_pool resolves pair to created pool" \
    '@cmd:invoke "$FACTORY_CONTRACT_ID" get_pool --token_a "$ta" --token_b "$tb" | extract_contract_id' "$pool_addr"

  # ── get_lp_token returns a real LP token ────────────────────────────────
  local lp_token
  lp_token=$(e2e_read_until "factory: get_lp_token" \
    'invoke "$FACTORY_CONTRACT_ID" get_lp_token --pool "$pool_addr" | extract_contract_id' _nonempty)
  if [[ -z "$lp_token" ]]; then
    die "factory: get_lp_token returned no address for pool $pool_addr"
  fi
  pass "factory: get_lp_token returned a real LP token: $lp_token"

  # ── run the liquidity/swap flow against the factory-created pool ───────
  local amount_a=500000
  local amount_b=1000000
  local swap_amount_in=10000
  # amount_a/amount_b are in the pool's token order, not ta/tb's.
  local pool_ta pool_tb
  pool_ta=$(pool_token "$pool_addr" token_a)
  pool_tb=$(pool_token "$pool_addr" token_b)
  # add_liquidity spends all of amount_a, so mint the swap input on top.
  invoke "$pool_ta" mint --to "$admin" --amount "$(( amount_a + swap_amount_in ))" >/dev/null
  invoke "$pool_tb" mint --to "$admin" --amount "$amount_b" >/dev/null

  local deadline add_output lp_shares
  deadline=$(( $(date +%s) + 300 ))
  add_output=$(invoke "$pool_addr" add_liquidity \
    --provider "$admin" \
    --amount_a "$amount_a" \
    --amount_b "$amount_b" \
    --min_shares 0 \
    --deadline "$deadline")
  lp_shares=$(printf '%s\n' "$add_output" | parse_i128)
  assert_gt "factory: add_liquidity on factory pool returns LP shares" "$lp_shares" 0

  deadline=$(( $(date +%s) + 300 ))
  local swap_output swap_out
  swap_output=$(invoke "$pool_addr" swap \
    --trader "$admin" \
    --token_in "$pool_ta" \
    --amount_in "$swap_amount_in" \
    --min_out 0 \
    --deadline "$deadline")
  swap_out=$(printf '%s\n' "$swap_output" | parse_i128)
  assert_gt "factory: swap on factory pool returns positive output" "$swap_out" 0

  # ── set_default_fee_tier ────────────────────────────────────────────────
  invoke "$FACTORY_CONTRACT_ID" set_default_fee_tier --fee_tier 1 >/dev/null
  pass "factory: set_default_fee_tier(1) succeeded"

  # ── pause_creation / unpause_creation ───────────────────────────────────
  invoke "$FACTORY_CONTRACT_ID" pause_creation --admin "$admin" >/dev/null
  pass "factory: pause_creation succeeded"

  local other_tb
  other_tb=$(invoke_read_new_token_pair)

  local paused_out=""
  local paused_rc=0
  paused_out=$(invoke "$FACTORY_CONTRACT_ID" create_pool_with_fee_bps \
    --caller "$admin" \
    --token_a "$ta" \
    --token_b "$other_tb" \
    --fee_bps 25 2>&1) || paused_rc=$?
  if [[ "$paused_rc" -eq 0 ]]; then
    die "factory: create_pool_with_fee_bps succeeded while creation was paused: $paused_out"
  fi
  pass "factory: pool creation is blocked while paused"

  invoke "$FACTORY_CONTRACT_ID" unpause_creation --admin "$admin" >/dev/null
  pass "factory: unpause_creation succeeded"

  local resumed
  resumed=$(invoke "$FACTORY_CONTRACT_ID" create_pool_with_fee_bps \
    --caller "$admin" \
    --token_a "$ta" \
    --token_b "$other_tb" \
    --fee_bps 25 | extract_contract_id)
  if [[ -z "$resumed" ]]; then
    die "factory: pool creation did not resume after unpause_creation"
  fi
  pass "factory: pool creation resumed after unpause: $resumed"
}

# A throwaway third token so the pause/unpause check creates a pair that
# does not collide with the pool created earlier in this flow.
invoke_read_new_token_pair() {
  local admin="$SOURCE_PUBLIC_KEY"
  local wasm
  wasm=$(e2e_wasm token)
  local id
  id=$(stellar contract deploy \
    --wasm "$wasm" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT" | extract_contract_id)
  if [[ -z "$id" ]]; then
    die "factory: failed to deploy throwaway token for pause/unpause check"
  fi
  invoke "$id" initialize \
    --admin "$admin" \
    --name "E2E Throwaway" \
    --symbol "E2ET" \
    --decimals 7 >/dev/null
  printf '%s' "$id"
}
