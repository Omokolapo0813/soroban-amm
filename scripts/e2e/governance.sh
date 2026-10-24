#!/usr/bin/env bash
# governance.sh — end-to-end flow for the governance path:
# propose -> vote -> advance past timelock -> execute, asserting proposal
# status at each stage via proposal_status().
#
# Deploys its own fresh throwaway token pair, AMM pool (via the shared
# factory), and governance instance — like factory.sh, this keeps its
# pause/fee-tier and voting-window mutations from ever touching the
# shared AMM_POOL_CONTRACT_ID / GOVERNANCE_CONTRACT_ID that other flows
# (and manual testing) rely on. voting_period_secs and timelock_secs are
# also set as short as the contract allows so the flow doesn't have to
# wait out the real 7-day/2-day production defaults.
#
# IMPORTANT — two real constraints this flow cannot work around:
#
# 1. governance::execute() enforces `execute_after = vote_end +
#    max(timelock_secs, VETO_WINDOW_SECS)`, and VETO_WINDOW_SECS is a
#    hardcoded 24-hour constant in contracts/governance/src/lib.rs — not
#    configurable via `initialize`. Even with voting_period_secs=1 and
#    timelock_secs=1, a proposal cannot be executed until at least 24
#    real-world hours after voting closes. This script sleeps for the
#    actual required duration (computed from get_proposal's vote_end /
#    execute_after), so a full run genuinely takes 24h+ against a live
#    network. Set GOVERNANCE_SKIP_EXECUTE=1 to assert only Active ->
#    Defeated/Queued and skip the execute step and its multi-hour sleep.
#
# 2. vote() requires the LP token's `Locker` to point at this governance
#    instance (LpToken::lock() checks `locker.require_auth()`). Only the pool
#    can change the locker, so the flow asks it to through the pool's
#    admin-gated `set_lp_locker`, signed by the pool admin, then reads the
#    locker back before proposing (issue #986).
set -Eeuo pipefail

run_governance_flow() {
  CURRENT_FLOW="governance"

  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then
    die "governance: FACTORY_CONTRACT_ID not set"
  fi

  local admin="$SOURCE_PUBLIC_KEY"
  # Long enough for propose, a status read and vote to land before voting
  # closes: each is its own transaction, and testnet closes a ledger ~5s.
  local voting_period="${GOVERNANCE_VOTING_PERIOD_SECS:-60}"
  local timelock="${GOVERNANCE_TIMELOCK_SECS:-5}"
  local quorum_bps="${GOVERNANCE_QUORUM_BPS:-1000}"
  local min_proposer_stake_bps="${GOVERNANCE_MIN_PROPOSER_STAKE_BPS:-1}"

  # ── isolated token pair + AMM pool ───────────────────────────────────────
  local ta tb
  ta=$(deploy_throwaway_token "GovA")
  tb=$(deploy_throwaway_token "GovB")

  local pool_addr lp_token
  pool_addr=$(invoke "$FACTORY_CONTRACT_ID" create_pool_with_fee_bps \
    --caller "$admin" \
    --token_a "$ta" \
    --token_b "$tb" \
    --fee_bps 30 | extract_contract_id)
  if [[ -z "$pool_addr" ]]; then
    die "governance: failed to create isolated AMM pool"
  fi
  lp_token=$(e2e_read_until "governance: get_lp_token" \
    'invoke "$FACTORY_CONTRACT_ID" get_lp_token --pool "$pool_addr" | extract_contract_id' _nonempty)
  if [[ -z "$lp_token" ]]; then
    die "governance: could not resolve LP token for isolated pool"
  fi
  pass "governance: deployed isolated AMM pool $pool_addr (lp=$lp_token)"

  # Seed LP supply so the proposer clears min_proposer_stake_bps and quorum.
  local amount=1000000
  invoke "$ta" mint --to "$admin" --amount "$amount" >/dev/null
  invoke "$tb" mint --to "$admin" --amount "$amount" >/dev/null
  local deadline
  deadline=$(( $(date +%s) + 300 ))
  invoke "$pool_addr" add_liquidity \
    --provider "$admin" \
    --amount_a "$amount" \
    --amount_b "$amount" \
    --min_shares 0 \
    --deadline "$deadline" >/dev/null
  pass "governance: seeded LP supply on isolated pool"

  # ── isolated governance instance ─────────────────────────────────────────
  local gov_wasm
  gov_wasm=$(e2e_wasm governance)
  local governance
  governance=$(stellar contract deploy \
    --wasm "$gov_wasm" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT" | extract_contract_id)
  if [[ -z "$governance" ]]; then
    die "governance: failed to deploy isolated governance instance"
  fi
  invoke "$governance" initialize \
    --admin "$admin" \
    --amm_pool "$pool_addr" \
    --lp_token "$lp_token" \
    --voting_period_secs "$voting_period" \
    --timelock_secs "$timelock" \
    --quorum_bps "$quorum_bps" \
    --min_proposer_stake_bps "$min_proposer_stake_bps" >/dev/null
  pass "governance: deployed and initialized isolated governance: $governance"

  # ── LP token locker -> governance ─────────────────────────────────────────
  # The isolated pool's admin is the factory admin (this account), so it can
  # have the pool delegate the locker directly.
  invoke "$pool_addr" set_lp_locker --locker "$governance" >/dev/null
  assert_eq "governance: LP token locker is the governance instance" \
    '@cmd:invoke "$lp_token" locker | extract_contract_id' "$governance"

  # ── propose ──────────────────────────────────────────────────────────────
  local proposal_id
  proposal_id=$(invoke "$governance" propose \
    --proposer "$admin" \
    --kind '{"UpdateFee":"25"}' | parse_i128)
  if [[ -z "$proposal_id" ]]; then
    die "governance: propose did not return a proposal id"
  fi
  pass "governance: created proposal $proposal_id (UpdateFee -> 25 bps)"

  local status
  assert_contains "governance: proposal status is Active" \
    '@cmd:invoke "$governance" proposal_status --proposal_id "$proposal_id"' "Active"

  # ── vote ─────────────────────────────────────────────────────────────────
  invoke "$governance" vote --voter "$admin" --proposal_id "$proposal_id" --choice '{"For":[]}' >/dev/null
  pass "governance: voted For on proposal $proposal_id"

  # Locking does not move the balance, so it is safe to read up front.
  local lp_balance
  lp_balance=$(invoke "$lp_token" balance --id "$admin" | parse_i128)
  assert_eq "governance: voter's LP balance is locked by the vote" \
    '@cmd:invoke "$lp_token" locked_balance --id "$admin" | parse_i128' "$lp_balance"

  # ── advance past voting period ───────────────────────────────────────────
  # Wait on the proposal's own vote_end rather than voting_period, since part
  # of the window was spent above; +6s lets a ledger past vote_end close.
  local vote_end
  vote_end=$(invoke "$governance" get_proposal --proposal_id "$proposal_id" | field_value vote_end)
  local vote_wait=$(( vote_end - $(date +%s) + 6 ))
  if (( vote_wait > 0 )); then
    sleep "$vote_wait"
  fi

  status=$(invoke "$governance" proposal_status --proposal_id "$proposal_id")
  pass "governance: proposal status after voting period: $status"
  if [[ "$status" == *"Defeated"* ]]; then
    die "governance: proposal was Defeated — voting power/quorum setup is wrong"
  fi

  if [[ "${GOVERNANCE_SKIP_EXECUTE:-0}" == "1" ]]; then
    pass "governance: GOVERNANCE_SKIP_EXECUTE=1 — skipping execute (see header comment on the 24h VETO_WINDOW_SECS floor)"
    return
  fi

  # ── advance past the timelock (dominated by the 24h VETO_WINDOW_SECS
  # floor — see header comment) and execute ──────────────────────────────
  local proposal_output execute_after now_ts wait_secs
  proposal_output=$(invoke "$governance" get_proposal --proposal_id "$proposal_id")
  execute_after=$(printf '%s\n' "$proposal_output" | field_value execute_after)
  now_ts=$(date +%s)
  wait_secs=$(( execute_after - now_ts + 2 ))
  if (( wait_secs > 0 )); then
    pass "governance: sleeping ${wait_secs}s until execute_after=$execute_after"
    sleep "$wait_secs"
  fi

  status=$(invoke "$governance" proposal_status --proposal_id "$proposal_id")
  if [[ "$status" != *"Queued"* ]]; then
    die "governance: proposal status expected Queued before execute, got: $status"
  fi
  pass "governance: proposal status is Queued"

  invoke "$governance" execute --proposal_id "$proposal_id" >/dev/null
  pass "governance: executed proposal $proposal_id"

  assert_contains "governance: proposal status is Executed" \
    '@cmd:invoke "$governance" proposal_status --proposal_id "$proposal_id"' "Executed"

  assert_eq "governance: pool fee_bps updated by executed proposal" \
    '@cmd:invoke "$pool_addr" get_info | field_value fee_bps' "25"

  # unlock_vote releases the LP tokens through LpToken::unlock, authorised by
  # governance as the locker that locked them.
  invoke "$governance" unlock_vote --voter "$admin" --proposal_id "$proposal_id" >/dev/null
  assert_eq "governance: LP balance unlocked after execution" \
    '@cmd:invoke "$lp_token" locked_balance --id "$admin" | parse_i128' "0"
}

# A throwaway token for the isolated governance pool, independent of the
# shared TOKEN_A/TOKEN_B so this flow never touches shared pool state.
deploy_throwaway_token() {
  local symbol="$1"
  local admin="$SOURCE_PUBLIC_KEY"
  local wasm
  wasm=$(e2e_wasm token)
  local id
  id=$(stellar contract deploy \
    --wasm "$wasm" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT" | extract_contract_id)
  if [[ -z "$id" ]]; then
    die "governance: failed to deploy throwaway token $symbol"
  fi
  invoke "$id" initialize \
    --admin "$admin" \
    --name "E2E $symbol" \
    --symbol "$symbol" \
    --decimals 7 >/dev/null
  printf '%s' "$id"
}
