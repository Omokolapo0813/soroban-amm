#!/usr/bin/env bash
# v2.sh — end-to-end flow for the constant-product (V2) AMM path:
# mint -> add_liquidity -> swap -> remove_liquidity, asserting real numeric
# outcomes at each step (not just exit codes).
set -Eeuo pipefail

# True once get_info's reserve_a differs from $2, i.e. a swap has landed.
_v2_reserve_a_moved() { [[ "$(printf '%s\n' "$1" | field_value reserve_a)" != "$2" ]]; }

run_v2_flow() {
  CURRENT_FLOW="v2"

  local amount_a="${AMOUNT_A:-1000000}"
  local amount_b="${AMOUNT_B:-2000000}"
  local swap_amount_in="${SWAP_AMOUNT_IN:-100000}"
  local min_swap_out="${MIN_SWAP_OUT:-150000}"
  local max_swap_out="${MAX_SWAP_OUT:-200000}"

  # amount_a/amount_b are in the pool's token order, not TOKEN_A/TOKEN_B's.
  local tok_a tok_b
  tok_a=$(pool_token "$AMM_CONTRACT_ID" token_a)
  tok_b=$(pool_token "$AMM_CONTRACT_ID" token_b)

  # add_liquidity spends all of amount_a, so mint the swap input on top.
  invoke "$tok_a" mint \
    --to "$SOURCE_PUBLIC_KEY" \
    --amount "$(( amount_a + swap_amount_in ))" >/dev/null
  pass "v2: minted token A to test account"

  invoke "$tok_b" mint \
    --to "$SOURCE_PUBLIC_KEY" \
    --amount "$amount_b" >/dev/null
  pass "v2: minted token B to test account"

  local deadline
  deadline=$(( $(date +%s) + 300 ))

  local add_output lp_shares
  add_output="$(invoke "$AMM_CONTRACT_ID" add_liquidity \
    --provider "$SOURCE_PUBLIC_KEY" \
    --amount_a "$amount_a" \
    --amount_b "$amount_b" \
    --min_shares 0 \
    --deadline "$deadline")"
  lp_shares="$(printf '%s\n' "$add_output" | parse_i128)"
  if [[ -z "$lp_shares" || "$lp_shares" -le 0 ]]; then
    die "v2: add_liquidity did not return positive LP shares: $add_output"
  fi
  pass "v2: added liquidity and received LP shares: $lp_shares"

  assert_eq "v2: reserve A after add_liquidity" \
    '@cmd:invoke "$AMM_CONTRACT_ID" get_info | field_value reserve_a' "$amount_a"
  assert_eq "v2: reserve B after add_liquidity" \
    '@cmd:invoke "$AMM_CONTRACT_ID" get_info | field_value reserve_b' "$amount_b"

  deadline=$(( $(date +%s) + 300 ))

  local swap_output swap_out
  swap_output="$(invoke "$AMM_CONTRACT_ID" swap \
    --trader "$SOURCE_PUBLIC_KEY" \
    --token_in "$tok_a" \
    --amount_in "$swap_amount_in" \
    --min_out 0 \
    --deadline "$deadline")"
  swap_out="$(printf '%s\n' "$swap_output" | parse_i128)"
  if [[ -z "$swap_out" ]]; then
    die "v2: swap did not return an amount: $swap_output"
  fi
  assert_between "v2: swap output" "$swap_out" "$min_swap_out" "$max_swap_out"

  # The first deposit permanently locks MINIMUM_LIQUIDITY shares, so removing
  # every share this flow holds leaves the pool with the locked shares and
  # their pro-rata slice of the reserves, not an empty pool.
  local pre_info pre_reserve_a pre_reserve_b pre_total
  # Wait for the swap to show in the reserves, or the baseline is pre-swap.
  pre_info="$(e2e_read_until "v2: get_info after the swap" \
    'invoke "$AMM_CONTRACT_ID" get_info' _v2_reserve_a_moved "$amount_a")"
  pre_reserve_a="$(printf '%s\n' "$pre_info" | field_value reserve_a)"
  pre_reserve_b="$(printf '%s\n' "$pre_info" | field_value reserve_b)"
  pre_total="$(printf '%s\n' "$pre_info" | field_value total_shares)"

  deadline=$(( $(date +%s) + 300 ))

  invoke "$AMM_CONTRACT_ID" remove_liquidity \
    --provider "$SOURCE_PUBLIC_KEY" \
    --shares "$lp_shares" \
    --min_a 0 \
    --min_b 0 \
    --deadline "$deadline" >/dev/null
  pass "v2: removed all LP shares"

  local remaining
  remaining=$(( pre_total - lp_shares ))
  assert_eq "v2: total_shares after removing this flow's shares" \
    '@cmd:invoke "$AMM_CONTRACT_ID" get_info | field_value total_shares' "$remaining"
  # Payouts round down, so the pool keeps at most one extra unit per reserve.
  local keep_a=$(( pre_reserve_a * remaining / pre_total ))
  local keep_b=$(( pre_reserve_b * remaining / pre_total ))
  assert_between "v2: final reserve A is the remaining shares' slice" \
    '@cmd:invoke "$AMM_CONTRACT_ID" get_info | field_value reserve_a' "$keep_a" "$(( keep_a + 1 ))"
  assert_between "v2: final reserve B is the remaining shares' slice" \
    '@cmd:invoke "$AMM_CONTRACT_ID" get_info | field_value reserve_b' "$keep_b" "$(( keep_b + 1 ))"
}
