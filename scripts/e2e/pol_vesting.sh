#!/usr/bin/env bash
# pol_vesting.sh — end-to-end flow for protocol-owned-liquidity vesting:
# initialize -> fund -> create_vesting -> release -> revoke_vesting,
# asserting the schedule, the released amount and the final split between
# beneficiary and treasury all read back consistently and conserve `total`.
#
# The schedule vests linearly from ledger 0 to u32::MAX, so on any live
# network a strictly partial amount has vested by the time `release` runs.
# That keeps the flow independent of reading the current ledger sequence.
#
# Self-contained: deploys its own LP token and vesting instance, so it runs
# without deploy.sh:
#   bash scripts/e2e/pol_vesting.sh
set -Eeuo pipefail

# schedule_released VEST BENEFICIARY ID - the `released` field of a schedule.
schedule_released() {
  invoke "$1" get_vesting --beneficiary "$2" --schedule_id "$3" | json_num released
}

run_pol_vesting_flow() {
  CURRENT_FLOW="pol_vesting"

  local governance="$SOURCE_PUBLIC_KEY"
  local beneficiary="$SOURCE_PUBLIC_KEY"
  local outsider_key="${SOURCE_ACCOUNT}-vest-outsider"
  local treasury outsider
  treasury=$(e2e_fund_account vest-treasury)
  outsider=$(e2e_fund_account vest-outsider)

  local total="${VEST_TOTAL:-10000000000000}"
  local end_ledger=4294967295

  # ── initialize ──────────────────────────────────────────────────────────
  # create_vesting checks that the LP token is administered by `pool`, so the
  # flow uses a real factory pool. Seeding it mints the LP shares to the source
  # account, which is both governance and the beneficiary here.
  local factory tx ty pool lp vest
  factory=$(e2e_new_factory)
  tx=$(e2e_new_token VTA)
  ty=$(e2e_new_token VTB)
  pool=$(e2e_new_seeded_pool "$factory" "$tx" "$ty" "$(( total * 2 ))")
  lp=$(e2e_read_until "pol_vesting: get_lp_token" \
    'invoke "$factory" get_lp_token --pool "$pool" | extract_contract_id' _nonempty)
  if [[ -z "$lp" ]]; then
    die "pol_vesting: factory returned no LP token for the seeded pool"
  fi
  vest=$(e2e_deploy pol_vesting)
  invoke "$vest" initialize --governance "$governance" --treasury "$treasury" >/dev/null
  assert_eq "pol_vesting: governance reads back" '@cmd:invoke "$vest" get_governance | parse_address' "$governance"
  assert_eq "pol_vesting: treasury reads back" '@cmd:invoke "$vest" get_treasury | parse_address' "$treasury"
  expect_fail "pol_vesting: second initialize is rejected" \
    invoke "$vest" initialize --governance "$outsider" --treasury "$outsider"

  # ── fund the vesting contract (create_vesting expects tokens in place) ──
  invoke "$lp" transfer --from "$governance" --to "$vest" --amount "$total" >/dev/null
  assert_eq_settled "pol_vesting: contract holds the vesting total" "$total" balance_of "$lp" "$vest"

  # ── create_vesting ──────────────────────────────────────────────────────
  expect_fail "pol_vesting: schedule with end <= cliff is rejected" \
    invoke "$vest" create_vesting \
      --governance "$governance" --beneficiary "$beneficiary" --lp_token "$lp" --pool "$pool" \
      --total "$total" --start_ledger 0 --cliff_ledger 100 --end_ledger 100
  expect_fail "pol_vesting: non-governance caller cannot create a schedule" \
    invoke_as "$outsider_key" "$vest" create_vesting \
      --governance "$outsider" --beneficiary "$outsider" --lp_token "$lp" --pool "$pool" \
      --total "$total" --start_ledger 0 --cliff_ledger 0 --end_ledger "$end_ledger"
  # `tx` is a token the pool did not issue, so it is not the pool's LP token.
  expect_fail "pol_vesting: pool that did not issue the LP token is rejected" \
    invoke "$vest" create_vesting \
      --governance "$governance" --beneficiary "$beneficiary" --lp_token "$lp" --pool "$tx" \
      --total "$total" --start_ledger 0 --cliff_ledger 0 --end_ledger "$end_ledger"

  local schedule_id
  schedule_id=$(invoke "$vest" create_vesting \
    --governance "$governance" \
    --beneficiary "$beneficiary" \
    --lp_token "$lp" \
    --pool "$pool" \
    --total "$total" \
    --start_ledger 0 \
    --cliff_ledger 0 \
    --end_ledger "$end_ledger" | parse_i128)
  assert_eq "pol_vesting: first schedule id" "$schedule_id" "0"

  local schedule
  schedule=$(invoke "$vest" get_vesting --beneficiary "$beneficiary" --schedule_id "$schedule_id")
  assert_eq "pol_vesting: schedule total reads back" '@cmd:printf '\''%s\n'\'' "$schedule" | json_num total' "$total"
  assert_eq "pol_vesting: schedule starts unreleased" '@cmd:printf '\''%s\n'\'' "$schedule" | json_num released' "0"
  assert_eq "pol_vesting: schedule end_ledger reads back" '@cmd:printf '\''%s\n'\'' "$schedule" | json_num end_ledger' "$end_ledger"

  # ── release: a strictly partial amount has vested ───────────────────────
  local bal0 released
  bal0=$(balance_of "$lp" "$beneficiary")
  released=$(invoke "$vest" release --beneficiary "$beneficiary" --schedule_id "$schedule_id" | parse_i128)
  assert_gt "pol_vesting: release pays out a positive amount" "$released" 0
  if (( released >= total )); then
    die "pol_vesting: release paid out $released, expected strictly less than total $total"
  fi
  pass "pol_vesting: release is partial ($released < $total)"
  assert_eq_settled "pol_vesting: beneficiary credited by release" "$(( bal0 + released ))" balance_of "$lp" "$beneficiary"
  assert_eq_settled "pol_vesting: schedule records the release" "$released" \
    schedule_released "$vest" "$beneficiary" "$schedule_id"

  # ── revoke_vesting: remaining tokens split, total conserved ─────────────
  # Baselines come from checks that already settled, not from a fresh read
  # taken right after a write. Only the treasury read follows a state change
  # (the revoke), so it is polled until the remainder shows up.
  local bal1 treasury0 to_beneficiary to_treasury attempt
  bal1=$(( bal0 + released ))
  treasury0=$(balance_of "$lp" "$treasury")
  invoke "$vest" revoke_vesting \
    --governance "$governance" --beneficiary "$beneficiary" --schedule_id "$schedule_id" >/dev/null
  for attempt in 1 2 3 4 5 6; do
    to_treasury=$(( $(balance_of "$lp" "$treasury") - treasury0 ))
    (( to_treasury > 0 )) && break
    printf '[retry] pol_vesting: treasury balance has not moved yet; waiting in case the RPC node lags\n' >&2
    sleep 6
  done
  to_beneficiary=$(( $(balance_of "$lp" "$beneficiary") - bal1 ))
  assert_eq "pol_vesting: revoke does not pay beneficiary directly" "$to_beneficiary" "0"
  assert_gt "pol_vesting: treasury receives the unvested remainder" "$to_treasury" 0

  assert_contains_settled "pol_vesting: schedule is marked revoked" "true" \
    invoke "$vest" get_vesting --beneficiary "$beneficiary" --schedule_id "$schedule_id"

  # ── release after revoke: beneficiary claims frozen remainder ───────────
  local claimable bal2 remainder_released
  claimable=$(invoke "$vest" releasable_amount --beneficiary "$beneficiary" --schedule_id "$schedule_id" | parse_i128)
  assert_gt "pol_vesting: claimable remainder after revoke is positive" "$claimable" 0
  bal2=$bal1  # revoke paid the beneficiary nothing (asserted above)
  remainder_released=$(invoke "$vest" release --beneficiary "$beneficiary" --schedule_id "$schedule_id" | parse_i128)
  assert_eq "pol_vesting: release pays out frozen remainder" "$remainder_released" "$claimable"
  assert_eq_settled "pol_vesting: beneficiary credited by remainder release" "$(( bal2 + remainder_released ))" balance_of "$lp" "$beneficiary"
  assert_eq "pol_vesting: released + remainder + unvested equals total" \
    "$(( released + remainder_released + to_treasury ))" "$total"
  assert_eq_settled "pol_vesting: contract is emptied after remainder release" "0" balance_of "$lp" "$vest"
  expect_fail "pol_vesting: second release has nothing to release" \
    invoke "$vest" release --beneficiary "$beneficiary" --schedule_id "$schedule_id"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_pol_vesting_flow
fi
