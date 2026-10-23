use std::{env as std_env, fs, process};

mod report;

use report::{
    check_headroom, check_regressions, check_stale_baseline, parse_allowlist, parse_baseline,
    render_json, render_summary, Metric, DEFAULT_HEADROOM_PCT,
};

use amm::{AmmPool, AmmPoolClient};
use batch_auction::{BatchAuction, BatchAuctionClient};
use batch_router::{BatchOp, BatchRouter, BatchRouterClient, PoolType as BatchPoolType, SwapOp};
use concentrated_liquidity::{ConcentratedLiquidity, ConcentratedLiquidityClient};
use dex_aggregator::{DexAggregator, DexAggregatorClient};
use factory::{Factory, FactoryClient};
use governance::{Governance, GovernanceClient, ProposalKind, Vote};
use incentive_campaigns::{IncentiveCampaigns, IncentiveCampaignsClient};
use router::{Router, RouterClient};
use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient as StellarTokenClient},
    Address, Bytes, BytesN, Env, String as SorobanString, Vec as SorobanVec,
};
use staking::{Staking, StakingClient};
use token::{LpToken, LpTokenClient};
use twal_consumer::{PoolType as TwalPoolType, TwalConsumer, TwalConsumerClient};
use twap_consumer::{TwapConsumer, TwapConsumerClient};

const BASELINE_PATH: &str = "benches/baseline.json";
const ALLOWLIST_PATH: &str = "benches/allowlist.json";

/// Where the optimized pool WASM is looked for after `BENCH_WASM_DIR`: the directory CI
/// writes it to, then the one `scripts/optimize_contracts.sh` writes it to.
const OPTIMIZED_WASM_DIRS: [&str; 2] = [
    "target/wasm32v1-none/release/optimized",
    "optimized-artifacts",
];

/// Contracts the factory deploys from compiled WASM, so their calls pay for loading the
/// module into the VM.
const FACTORY_DEPLOYED_WASM: [&str; 2] = ["amm", "token"];

/// Path of the optimized build of `name`, if there is one.
fn optimized_wasm_path(name: &str) -> Option<String> {
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(dir) = std_env::var("BENCH_WASM_DIR") {
        dirs.push(dir);
    }
    dirs.extend(OPTIMIZED_WASM_DIRS.iter().map(|dir| dir.to_string()));
    dirs.into_iter()
        .map(|dir| format!("{dir}/{name}.wasm"))
        .find(|path| std::path::Path::new(path).is_file())
}

/// The optimized build of `name` when present, else the raw build embedded in the crate.
///
/// Pools made by the factory run as WASM, and the cost of a call is dominated by loading
/// the module, which scales with its size. The raw `amm` build is over the network's
/// 128KB contract cap and can never be deployed, so benchmarking it overstates every
/// routing path by about 20%. The optimized build is what ships.
fn pool_wasm(name: &str, embedded: &'static [u8]) -> Vec<u8> {
    match optimized_wasm_path(name) {
        Some(path) => fs::read(&path).unwrap_or_else(|err| panic!("read {path}: {err}")),
        None => embedded.to_vec(),
    }
}

#[contracttype]
enum ReceiverDataKey {
    Amm,
    TokenA,
    TokenB,
    ShouldRepay,
}

#[contract]
struct BenchFlashLoanReceiver;

#[contractimpl]
impl BenchFlashLoanReceiver {
    pub fn initialize(
        env: Env,
        amm: Address,
        token_a: Address,
        token_b: Address,
        should_repay: bool,
    ) {
        env.storage().instance().set(&ReceiverDataKey::Amm, &amm);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::TokenA, &token_a);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::TokenB, &token_b);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::ShouldRepay, &should_repay);
    }

    pub fn on_flash_loan(
        env: Env,
        amount_a: i128,
        amount_b: i128,
        fee_a: i128,
        fee_b: i128,
        _data: Bytes,
    ) -> bool {
        let should_repay = env
            .storage()
            .instance()
            .get(&ReceiverDataKey::ShouldRepay)
            .unwrap_or(false);
        if should_repay {
            let amm: Address = env.storage().instance().get(&ReceiverDataKey::Amm).unwrap();

            if amount_a > 0 || fee_a > 0 {
                let token_a: Address = env
                    .storage()
                    .instance()
                    .get(&ReceiverDataKey::TokenA)
                    .unwrap();
                StellarTokenClient::new(&env, &token_a).transfer(
                    &env.current_contract_address(),
                    &amm,
                    &(amount_a + fee_a),
                );
            }
            if amount_b > 0 || fee_b > 0 {
                let token_b: Address = env
                    .storage()
                    .instance()
                    .get(&ReceiverDataKey::TokenB)
                    .unwrap();
                StellarTokenClient::new(&env, &token_b).transfer(
                    &env.current_contract_address(),
                    &amm,
                    &(amount_b + fee_b),
                );
            }
        }
        true
    }
}

fn main() {
    let args: Vec<String> = std_env::args().collect();
    let missing: Vec<&str> = FACTORY_DEPLOYED_WASM
        .iter()
        .copied()
        .filter(|name| optimized_wasm_path(name).is_none())
        .collect();
    if !missing.is_empty() {
        let gated = args
            .iter()
            .any(|arg| arg == "--check" || arg == "--write-baseline");
        eprintln!(
            "benches: no optimized WASM for {}. The baseline is measured against the optimized \
             build, the one that gets deployed. Run `make bench`, or optimize them into \
             target/wasm32v1-none/release/optimized/ (`stellar contract optimize --wasm \
             target/wasm32v1-none/release/<name>.wasm --wasm-out \
             target/wasm32v1-none/release/optimized/<name>.wasm`), or set BENCH_WASM_DIR.",
            missing.join(", ")
        );
        if gated {
            process::exit(2);
        }
        eprintln!("benches: continuing with the raw build; these numbers are not comparable with the baseline.");
    }
    let metrics = run_all();
    let json = render_json(&metrics);

    if args.iter().any(|arg| arg == "--write-baseline") {
        fs::write(BASELINE_PATH, json).expect("write baseline");
        return;
    }

    let baseline = fs::read_to_string(BASELINE_PATH)
        .map_err(|err| format!("read {BASELINE_PATH}: {err}"))
        .and_then(|text| parse_baseline(&text));
    let table = render_summary(&metrics, baseline.as_ref().unwrap_or(&Default::default()));

    if args.iter().any(|arg| arg == "--summary") {
        // Markdown for `$GITHUB_STEP_SUMMARY`; the JSON stays on stderr-free stdout otherwise.
        print!("{table}");
    } else {
        println!("{json}");
    }

    if args.iter().any(|arg| arg == "--check") {
        if let Ok(path) = std_env::var("GITHUB_STEP_SUMMARY") {
            if let Err(err) = append_summary(&path, &table) {
                eprintln!("benches: could not write the step summary: {err}");
            }
        }
        if let Err(message) = run_checks(&metrics, baseline, &args) {
            eprintln!("{message}");
            process::exit(1);
        }
    }
}

/// Appends to the file `$GITHUB_STEP_SUMMARY` points at (other steps write to it too).
fn append_summary(path: &str, table: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "### Hot-path benchmarks\n")?;
    file.write_all(table.as_bytes())
}

fn run_checks(
    metrics: &[Metric],
    baseline: Result<report::Baseline, String>,
    args: &[String],
) -> Result<(), String> {
    let baseline = baseline?;
    let allowlist = match fs::read_to_string(ALLOWLIST_PATH) {
        Ok(text) => parse_allowlist(&text)?,
        Err(err) => return Err(format!("read {ALLOWLIST_PATH}: {err}")),
    };
    let headroom = headroom_pct(args)?;
    check_regressions(metrics, &baseline)?;
    check_stale_baseline(metrics, &baseline)?;
    check_headroom(metrics, headroom, &allowlist)
}

/// `--headroom-pct N` overrides the default share of the instruction limit.
fn headroom_pct(args: &[String]) -> Result<u64, String> {
    match args.iter().position(|arg| arg == "--headroom-pct") {
        None => Ok(DEFAULT_HEADROOM_PCT),
        Some(i) => args
            .get(i + 1)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| "--headroom-pct needs a whole number of percent".to_string()),
    }
}

fn run_all() -> Vec<Metric> {
    let mut metrics = vec![
        measure("amm.swap", bench_amm_swap),
        measure("amm.add_liquidity", bench_amm_add_liquidity),
        measure("amm.remove_liquidity", bench_amm_remove_liquidity),
        measure("amm.flash_loan", bench_amm_flash_loan),
        measure("cl.mint_position", bench_cl_mint_position),
        measure("cl.swap", bench_cl_swap),
        measure("batch.settle_batch", bench_batch_settle),
        measure("governance.propose", bench_governance_propose),
        measure("governance.vote", bench_governance_vote),
        measure("governance.execute", bench_governance_execute),
        measure("staking.stake", bench_staking_stake),
        measure("staking.claim", bench_staking_claim),
        measure(
            "incentive_campaigns.claim_rewards",
            bench_incentive_claim_rewards,
        ),
    ];

    // Paths whose cost scales with input size are measured at several sizes so
    // the baseline records the slope, not just a single point.
    for hops in HOP_COUNTS {
        metrics.push(measure(
            format!("router.swap_exact_in.hops_{hops}"),
            move |env| bench_router_swap_exact_in(env, hops),
        ));
    }
    for hops in HOP_COUNTS {
        metrics.push(measure(
            format!("dex_aggregator.execute_route.hops_{hops}"),
            move |env| bench_aggregator_execute_route(env, hops),
        ));
    }
    metrics.push(measure(
        "dex_aggregator.swap_best.hops_2",
        bench_aggregator_swap_best,
    ));
    for ticks in TICK_CROSSINGS {
        metrics.push(measure(
            format!("cl.swap_exact_out.ticks_{ticks}"),
            move |env| bench_cl_swap_exact_out(env, ticks),
        ));
    }
    for ticks in TICK_CROSSINGS {
        metrics.push(measure(
            format!("cl.burn_position.ticks_{ticks}"),
            move |env| bench_cl_burn_position(env, ticks),
        ));
    }
    for ticks in TICK_CROSSINGS {
        metrics.push(measure(
            format!("cl.collect_fees.ticks_{ticks}"),
            move |env| bench_cl_collect_fees(env, ticks),
        ));
    }
    metrics.push(measure("factory.create_pool", bench_factory_create_pool));

    // Hot paths whose cost grows with stored state, at the small and the
    // large end of what the contract allows.
    for ops in BATCH_OPS {
        metrics.push(measure(
            format!("batch_router.execute_batch.ops_{ops}"),
            move |env| bench_batch_router_execute(env, ops),
        ));
    }
    for pools in TRACKED_POOLS {
        metrics.push(measure(
            format!("twap_consumer.get_twap_all.pools_{pools}"),
            move |env| bench_twap_get_all(env, pools),
        ));
    }
    for pools in TRACKED_POOLS {
        metrics.push(measure(
            format!("twal_consumer.get_twal_all.pools_{pools}"),
            move |env| bench_twal_get_all(env, pools),
        ));
    }
    for stakers in STAKER_COUNTS {
        metrics.push(measure(
            format!("staking.stake.stakers_{stakers}"),
            move |env| bench_staking_stake_scaled(env, stakers),
        ));
    }
    metrics
}

/// Hop counts measured for multi-hop routing (router and aggregator).
const HOP_COUNTS: [u32; 3] = [1, 2, 3];
/// Number of initialized ticks crossed for the tick-walk-dependent CL paths.
const TICK_CROSSINGS: [i32; 3] = [1, 4, 8];
/// Swaps in one `batch_router.execute_batch`. Each swap runs the pool's WASM at
/// roughly 10.6M instructions, so about nine fit in one transaction: the
/// contract's ceiling of 200 operations is far past what the network allows.
/// The sizes stay under the headroom threshold; extrapolate the slope.
const BATCH_OPS: [u32; 2] = [2, 4];
/// Tracked pools read by `twap_consumer.get_twap_all` / `twal_consumer.get_twal_all`
/// (the consumers cap the set at 100).
const TRACKED_POOLS: [u32; 2] = [10, 100];
/// Stakers already registered when one more stakes.
const STAKER_COUNTS: [u32; 2] = [50, 400];

fn measure(name: impl Into<String>, f: impl FnOnce(&Env)) -> Metric {
    let name = name.into();
    eprintln!("running {name}");
    let env = Env::default();
    env.mock_all_auths();
    env.budget().reset_unlimited();
    f(&env);
    let (cpu_instructions, mem_bytes) = parse_budget(&format!("{}", env.budget()));
    Metric {
        name,
        cpu_instructions,
        mem_bytes,
    }
}

fn setup_amm(env: &Env, flash_fee_bps: i128) -> (AmmPoolClient<'_>, Address, Address, Address) {
    let admin = Address::generate(env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let lp = env.register_contract(None, LpToken);
    let amm = env.register_contract(None, AmmPool);
    LpTokenClient::new(env, &lp).initialize(
        &amm,
        &SorobanString::from_str(env, "LP"),
        &SorobanString::from_str(env, "LP"),
        &7,
    );
    let client = AmmPoolClient::new(env, &amm);
    client.initialize_with_flash_loan_fee(
        &admin,
        &token_a,
        &token_b,
        &lp,
        &30,
        &admin,
        &0,
        &flash_fee_bps,
    );

    let provider = Address::generate(env);
    StellarAssetClient::new(env, &token_a).mint(&provider, &2_000_000);
    StellarAssetClient::new(env, &token_b).mint(&provider, &2_000_000);
    client.add_liquidity(&provider, &1_000_000, &1_000_000, &0, &u64::MAX);
    (client, amm, token_a, token_b)
}

fn bench_amm_swap(env: &Env) {
    let (client, _, token_a, _) = setup_amm(env, 0);
    let trader = Address::generate(env);
    StellarAssetClient::new(env, &token_a).mint(&trader, &100_000);
    env.budget().reset_unlimited();
    client.swap(&trader, &token_a, &100_000, &0, &u64::MAX);
}

fn bench_amm_add_liquidity(env: &Env) {
    let (client, _, token_a, token_b) = setup_amm(env, 0);
    let provider = Address::generate(env);
    StellarAssetClient::new(env, &token_a).mint(&provider, &500_000);
    StellarAssetClient::new(env, &token_b).mint(&provider, &500_000);
    env.budget().reset_default();
    client.add_liquidity(&provider, &500_000, &500_000, &0, &u64::MAX);
}

fn bench_amm_remove_liquidity(env: &Env) {
    let (client, _, _, _) = setup_amm(env, 0);
    let provider = Address::generate(env);
    let info = client.get_info();
    StellarAssetClient::new(env, &info.token_a).mint(&provider, &500_000);
    StellarAssetClient::new(env, &info.token_b).mint(&provider, &500_000);
    let shares = client.add_liquidity(&provider, &500_000, &500_000, &0, &u64::MAX);
    env.budget().reset_default();
    client.remove_liquidity(&provider, &(shares / 2), &0, &0, &u64::MAX);
}

fn bench_amm_flash_loan(env: &Env) {
    let (client, amm, token_a, token_b) = setup_amm(env, 50);
    let receiver_addr = env.register_contract(None, BenchFlashLoanReceiver);
    let receiver = BenchFlashLoanReceiverClient::new(env, &receiver_addr);
    receiver.initialize(&amm, &token_a, &token_b, &true);
    StellarAssetClient::new(env, &token_a).mint(&receiver_addr, &1_000);
    env.budget().reset_default();
    client.flash_loan(&receiver_addr, &100_000_i128, &0_i128, &Bytes::new(env));
}

fn setup_cl(env: &Env) -> (ConcentratedLiquidityClient<'_>, Address, Address, Address) {
    let admin = Address::generate(env);
    let provider = Address::generate(env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let cl = env.register_contract(None, ConcentratedLiquidity);
    let client = ConcentratedLiquidityClient::new(env, &cl);
    client.initialize(&admin, &token_a, &token_b, &30, &0, &1);
    StellarAssetClient::new(env, &token_a).mint(&provider, &10_000_000);
    StellarAssetClient::new(env, &token_b).mint(&provider, &10_000_000);
    (client, provider, token_a, token_b)
}

fn bench_cl_mint_position(env: &Env) {
    let (client, provider, _, _) = setup_cl(env);
    env.budget().reset_default();
    client.mint_position(
        &provider,
        &-100,
        &100,
        &100_000,
        &100_000,
        &0,
        &0,
        &u64::MAX,
    );
}

fn bench_cl_swap(env: &Env) {
    let (client, provider, token_a, _) = setup_cl(env);
    client.mint_position(
        &provider,
        &-100,
        &100,
        &100_000,
        &100_000,
        &0,
        &0,
        &u64::MAX,
    );
    StellarAssetClient::new(env, &token_a).mint(&provider, &100);
    env.budget().reset_default();
    client.swap(&provider, &true, &100, &0, &0, &u64::MAX);
}

// ── Governance ────────────────────────────────────────────────────────────────

/// Deploy governance over a backing AMM pool, seed two LP holders, and open a
/// proposal. Returns `(governance client, proposal id, lp1, lp2)`.
fn setup_governance(env: &Env) -> (GovernanceClient<'_>, u32, Address, Address) {
    env.ledger().set_timestamp(1_000_000);
    let admin = Address::generate(env);

    let lp = env.register_contract(None, LpToken);
    LpTokenClient::new(env, &lp).initialize(
        &admin,
        &SorobanString::from_str(env, "LP"),
        &SorobanString::from_str(env, "LP"),
        &7,
    );

    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let gov_addr = env.register_contract(None, Governance);
    let amm = env.register_contract(None, AmmPool);
    AmmPoolClient::new(env, &amm).initialize(&gov_addr, &token_a, &token_b, &lp, &30, &admin, &0);

    let gov = GovernanceClient::new(env, &gov_addr);
    gov.initialize(
        &admin,
        &amm,
        &lp,
        &(7 * 24 * 60 * 60_u64), // voting_period_secs
        &(2 * 24 * 60 * 60_u64), // timelock_secs
        &1_000_i128,             // quorum_bps
        &100_i128,               // min_proposer_stake_bps
    );
    LpTokenClient::new(env, &lp).set_locker(&gov_addr);

    // Seed voting weight.
    let lp1 = Address::generate(env);
    let lp2 = Address::generate(env);
    LpTokenClient::new(env, &lp).mint(&lp1, &600);
    LpTokenClient::new(env, &lp).mint(&lp2, &400);

    let proposal_id = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
    (gov, proposal_id, lp1, lp2)
}

fn bench_governance_propose(env: &Env) {
    let (gov, _, lp1, _) = setup_governance(env);
    env.budget().reset_default();
    gov.propose(&lp1, &ProposalKind::UpdateFee(60));
}

fn bench_governance_vote(env: &Env) {
    let (gov, pid, lp1, _) = setup_governance(env);
    env.budget().reset_default();
    gov.vote(&lp1, &pid, &Vote::For);
}

fn bench_governance_execute(env: &Env) {
    let (gov, pid, lp1, lp2) = setup_governance(env);
    gov.vote(&lp1, &pid, &Vote::For);
    gov.vote(&lp2, &pid, &Vote::For);

    // Advance past the voting period and the timelock.
    let proposal = gov.get_proposal(&pid);
    env.ledger().set_timestamp(proposal.execute_after + 1);

    env.budget().reset_default();
    gov.execute(&pid);
}

// ── Staking ───────────────────────────────────────────────────────────────────

/// Deploy staking over an LP token and a reward token, fund the reward pool,
/// and mint LP to a staker. Returns `(staking client, staker, admin)`.
fn setup_staking(env: &Env) -> (StakingClient<'_>, Address, Address) {
    let admin = Address::generate(env);

    let lp = env.register_contract(None, LpToken);
    LpTokenClient::new(env, &lp).initialize(
        &admin,
        &SorobanString::from_str(env, "LP"),
        &SorobanString::from_str(env, "LP"),
        &7,
    );

    let reward_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let staking_addr = env.register_contract(None, Staking);
    let staking = StakingClient::new(env, &staking_addr);
    staking.initialize(&lp, &reward_token, &admin);

    // Fund the reward pool so `claim` has something to pay out.
    StellarAssetClient::new(env, &reward_token).mint(&admin, &1_000_000);
    staking.add_rewards(&admin, &1_000_000);

    let staker = Address::generate(env);
    LpTokenClient::new(env, &lp).mint(&staker, &1_000_000);
    (staking, staker, admin)
}

fn bench_staking_stake(env: &Env) {
    let (staking, staker, _) = setup_staking(env);
    env.budget().reset_default();
    staking.stake(&staker, &500_000);
}

fn bench_staking_claim(env: &Env) {
    let (staking, staker, admin) = setup_staking(env);
    staking.stake(&staker, &500_000);
    // `update_rewards` divides across current stakers, so it must run after the
    // stake for the staker to have a non-zero claimable balance.
    staking.update_rewards(&admin, &100_000);
    env.budget().reset_default();
    staking.claim(&staker);
}

// ── Incentive campaigns ───────────────────────────────────────────────────────

/// Deploy incentive_campaigns with a funded campaign over a backing pool, and
/// give a provider an LP balance to accrue against.
/// Returns `(client, campaign id, provider)`.
fn setup_incentive_campaigns(env: &Env) -> (IncentiveCampaignsClient<'_>, u64, Address) {
    env.ledger().set_timestamp(1_000);
    let governance = Address::generate(env);
    let admin = Address::generate(env);

    let reward_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let pool = Address::generate(env);

    // `create_campaign` requires the LP token's admin to be the backing pool.
    let lp = env.register_contract(None, LpToken);
    LpTokenClient::new(env, &lp).initialize(
        &pool,
        &SorobanString::from_str(env, "LP"),
        &SorobanString::from_str(env, "LP"),
        &7,
    );

    let campaigns_addr = env.register_contract(None, IncentiveCampaigns);
    let campaigns = IncentiveCampaignsClient::new(env, &campaigns_addr);
    campaigns.initialize(&governance);

    // Governance funds the campaign out of its own reward-token balance.
    StellarAssetClient::new(env, &reward_token).mint(&governance, &10_000_000);
    let campaign_id = campaigns.create_campaign(
        &governance,
        &pool,
        &lp,
        &reward_token,
        &1_000_u64,     // start_time
        &1_000_000_u64, // end_time
        &10_i128,       // reward_rate
        &10_000_000_i128,
    );

    let provider = Address::generate(env);
    LpTokenClient::new(env, &lp).mint(&provider, &1_000);
    // Campaigns are stake-based (#1115): rewards accrue to staked LP tokens.
    campaigns.stake(&provider, &campaign_id, &1_000_i128);

    // Advance so rewards have accrued for the provider to claim.
    env.ledger().set_timestamp(2_000);
    (campaigns, campaign_id, provider)
}

fn bench_incentive_claim_rewards(env: &Env) {
    let (campaigns, campaign_id, provider) = setup_incentive_campaigns(env);
    env.budget().reset_default();
    campaigns.claim_rewards(&provider, &campaign_id);
}

fn bench_batch_settle(env: &Env) {
    env.ledger().set_timestamp(1_000);
    let auction_addr = env.register_contract(None, BatchAuction);
    let admin = Address::generate(env);
    let auction = BatchAuctionClient::new(env, &auction_addr);
    auction.initialize(&admin, &30);
    env.ledger().set_timestamp(1_031);
    env.budget().reset_default();
    let _ = auction.try_settle_batch();
}

// ── Concentrated liquidity: tick-walk-dependent paths ─────────────────────────

/// Seed a CL pool with a wide base position plus `ticks` nested ladder
/// positions `[-10i, 10i]`, so a zero-for-one swap that moves the price below
/// `-10 * ticks` crosses exactly `ticks` initialized ticks. Returns the client
/// and the provider that owns every position.
fn setup_cl_ladder(env: &Env, ticks: i32) -> (ConcentratedLiquidityClient<'_>, Address) {
    let (client, provider, _, _) = setup_cl(env);
    client.mint_position(
        &provider,
        &-2_000,
        &2_000,
        &1_000_000,
        &1_000_000,
        &0,
        &0,
        &u64::MAX,
    );
    for i in 1..=ticks {
        client.mint_position(
            &provider,
            &(-10 * i),
            &(10 * i),
            &20_000,
            &20_000,
            &0,
            &0,
            &u64::MAX,
        );
    }
    (client, provider)
}

/// Sqrt-price limit just past the last ladder tick, so a swap stops after
/// crossing every one of them and no further.
fn cl_ladder_limit(ticks: i32) -> u128 {
    ConcentratedLiquidity::tick_to_sqrt_price_x96(-10 * ticks - 5)
}

/// Swap zero-for-one across every ladder tick so positions accrue fees and the
/// crossed ticks' fee-growth-outside values are flipped.
fn cross_cl_ladder(client: &ConcentratedLiquidityClient<'_>, provider: &Address, ticks: i32) {
    client.swap(
        provider,
        &true,
        &1_000_000,
        &cl_ladder_limit(ticks),
        &0,
        &u64::MAX,
    );
    assert!(
        client.get_pool_state().current_tick < -10 * ticks,
        "ladder swap did not cross all {ticks} ticks"
    );
}

fn bench_cl_swap_exact_out(env: &Env, ticks: i32) {
    // Learn how much output a swap to the ladder limit produces on an
    // identical pool, then request slightly less of it, so the measured
    // exact-out walk crosses the same initialized ticks.
    let probe_env = Env::default();
    probe_env.mock_all_auths();
    probe_env.budget().reset_unlimited();
    let (probe, probe_provider) = setup_cl_ladder(&probe_env, ticks);
    let full_out = probe.swap(
        &probe_provider,
        &true,
        &1_000_000,
        &cl_ladder_limit(ticks),
        &0,
        &u64::MAX,
    );
    let amount_out = full_out * 95 / 100;

    let (client, provider) = setup_cl_ladder(env, ticks);
    env.budget().reset_default();
    client.swap_exact_out(&provider, &true, &amount_out, &0, &i128::MAX, &u64::MAX);
}

fn bench_cl_burn_position(env: &Env, ticks: i32) {
    let (client, provider) = setup_cl_ladder(env, ticks);
    cross_cl_ladder(&client, &provider, ticks);
    let liquidity = client
        .get_position(&provider, &(-10 * ticks), &(10 * ticks))
        .liquidity;
    env.budget().reset_default();
    client.burn_position(&provider, &(-10 * ticks), &(10 * ticks), &liquidity);
}

fn bench_cl_collect_fees(env: &Env, ticks: i32) {
    let (client, provider) = setup_cl_ladder(env, ticks);
    cross_cl_ladder(&client, &provider, ticks);
    env.budget().reset_default();
    client.collect_fees(&provider, &-2_000, &2_000);
}

// ── Factory-deployed pools: router, aggregator, factory ───────────────────────

/// A factory plus `hops` factory-deployed pools chained
/// `tokens[0] → tokens[1] → … → tokens[hops]`, each seeded with liquidity.
/// The trader holds a balance of `tokens[0]`.
struct PoolChain<'a> {
    factory: FactoryClient<'a>,
    admin: Address,
    trader: Address,
    tokens: SorobanVec<Address>,
}

/// A factory with the real AMM and LP-token WASM uploaded, so pools it deploys
/// run in the metered VM exactly as they would on-chain.
fn setup_factory(env: &Env) -> (FactoryClient<'_>, Address) {
    let admin = Address::generate(env);
    let amm_wasm = pool_wasm("amm", amm::WASM);
    let lp_wasm = pool_wasm("token", token::WASM);
    let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(&amm_wasm[..]);
    let lp_hash: BytesN<32> = env.deployer().upload_contract_wasm(&lp_wasm[..]);
    let factory_addr = env.register_contract(None, Factory);
    let factory = FactoryClient::new(env, &factory_addr);
    factory.initialize(&admin, &amm_hash, &lp_hash);
    (factory, admin)
}

fn setup_pool_chain(env: &Env, hops: u32) -> PoolChain<'_> {
    let (factory, admin) = setup_factory(env);
    let lp = Address::generate(env);
    let trader = Address::generate(env);
    let mut tokens = SorobanVec::new(env);
    for _ in 0..=hops {
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        StellarAssetClient::new(env, &token).mint(&lp, &10_000_000);
        tokens.push_back(token);
    }
    StellarAssetClient::new(env, &tokens.get(0).unwrap()).mint(&trader, &1_000_000);
    for i in 0..hops {
        let a = tokens.get(i).unwrap();
        let b = tokens.get(i + 1).unwrap();
        // fee_tier 2 = 30 bps; no governance contract.
        let (pool, _) = factory.create_pool(&admin, &a, &b, &2_i128, &None);
        AmmPoolClient::new(env, &pool).add_liquidity(&lp, &1_000_000, &1_000_000, &0, &u64::MAX);
    }
    PoolChain {
        factory,
        admin,
        trader,
        tokens,
    }
}

fn bench_router_swap_exact_in(env: &Env, hops: u32) {
    let chain = setup_pool_chain(env, hops);
    let router_addr = env.register_contract(None, Router);
    let router = RouterClient::new(env, &router_addr);
    router.initialize(&chain.admin, &chain.factory.address);
    env.budget().reset_default();
    router.swap_exact_in(&chain.trader, &chain.tokens, &10_000, &0, &u64::MAX);
}

fn setup_aggregator<'a>(env: &'a Env, chain: &PoolChain<'a>) -> DexAggregatorClient<'a> {
    let agg_addr = env.register_contract(None, DexAggregator);
    let agg = DexAggregatorClient::new(env, &agg_addr);
    agg.initialize(&chain.admin, &chain.factory.address);
    // Intermediate tokens are the routing candidates the BFS may pass through.
    let mut routing = SorobanVec::new(env);
    for i in 1..chain.tokens.len() - 1 {
        routing.push_back(chain.tokens.get(i).unwrap());
    }
    agg.set_routing_tokens(&routing);
    agg
}

fn bench_aggregator_execute_route(env: &Env, hops: u32) {
    let chain = setup_pool_chain(env, hops);
    let agg = setup_aggregator(env, &chain);
    let token_in = chain.tokens.get(0).unwrap();
    let token_out = chain.tokens.get(hops).unwrap();
    let route = agg.find_best_route(&token_in, &token_out, &10_000, &hops);
    assert_eq!(
        route.hops.len(),
        hops,
        "aggregator chose an unexpected route"
    );
    env.budget().reset_default();
    agg.execute_route(&route, &chain.trader, &10_000, &0, &u64::MAX);
}

fn bench_aggregator_swap_best(env: &Env) {
    let chain = setup_pool_chain(env, 2);
    let agg = setup_aggregator(env, &chain);
    let token_in = chain.tokens.get(0).unwrap();
    let token_out = chain.tokens.get(2).unwrap();
    env.budget().reset_default();
    agg.swap_best(&chain.trader, &token_in, &token_out, &10_000, &0, &u64::MAX);
}

fn bench_factory_create_pool(env: &Env) {
    let (factory, admin) = setup_factory(env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    env.budget().reset_default();
    factory.create_pool(&admin, &token_a, &token_b, &2_i128, &None);
}

fn parse_budget(text: &str) -> (u64, u64) {
    let mut cpu = 0;
    let mut mem = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Cpu limit:") {
            let mut used = rest.split("used:");
            let _ = used.next();
            cpu = used
                .next()
                .and_then(|s| s.split(';').next())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Mem limit:") {
            let mut used = rest.split("used:");
            let _ = used.next();
            mem = used
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    (cpu, mem)
}

// ── Scaling benches (#1167) ───────────────────────────────────────────────────

fn bench_staking_stake_scaled(env: &Env, stakers: u32) {
    let (staking, staker, admin) = setup_staking(env);
    let lp = staking.get_pool_info().lp_token;
    let _ = admin;
    for _ in 0..stakers {
        let other = Address::generate(env);
        LpTokenClient::new(env, &lp).mint(&other, &1_000);
        staking.stake(&other, &1_000);
    }
    env.budget().reset_default();
    staking.stake(&staker, &500_000);
}

/// A CL-pool stand-in for the TWAP/TWAL consumers: cheap, deterministic, and
/// identical for every tracked pool, so the bench measures the consumer.
#[contract]
struct BenchClPool;

#[contractimpl]
impl BenchClPool {
    pub fn get_tick_cumulative(env: Env) -> (i64, u64) {
        let now = env.ledger().timestamp();
        (100 * now as i64, now)
    }

    pub fn observe(env: Env, _seconds_ago: u64) -> i64 {
        100 * env.ledger().timestamp() as i64
    }

    pub fn active_liquidity(_env: Env) -> i128 {
        1_000_000
    }
}

fn bench_twap_get_all(env: &Env, pools: u32) {
    env.ledger().set_timestamp(10_000);
    let keeper = Address::generate(env);
    let consumer = TwapConsumerClient::new(env, &env.register_contract(None, TwapConsumer));
    consumer.initialize(&keeper);
    for _ in 0..pools {
        consumer.save_cl_snapshot(&env.register_contract(None, BenchClPool));
    }
    env.ledger().set_timestamp(10_060);
    env.budget().reset_default();
    let all = consumer.get_twap_all(&60);
    assert_eq!(all.len(), pools, "every tracked pool must be read");
}

fn bench_twal_get_all(env: &Env, pools: u32) {
    env.ledger().set_timestamp(10_000);
    let keeper = Address::generate(env);
    let consumer = TwalConsumerClient::new(env, &env.register_contract(None, TwalConsumer));
    consumer.initialize(&keeper);
    for _ in 0..pools {
        let pool = env.register_contract(None, BenchClPool);
        consumer.add_tracked_pool(&pool, &TwalPoolType::Cl);
        consumer.save_cl_snapshot(&pool);
    }
    env.ledger().set_timestamp(10_060);
    env.budget().reset_default();
    let all = consumer.get_twal_all(&60);
    assert_eq!(all.len(), pools, "every tracked pool must be read");
}

fn bench_batch_router_execute(env: &Env, ops: u32) {
    let chain = setup_pool_chain(env, 1);
    let router = BatchRouterClient::new(env, &env.register_contract(None, BatchRouter));
    router.initialize(&chain.admin, &chain.factory.address);
    let token_in = chain.tokens.get(0).unwrap();
    let token_out = chain.tokens.get(1).unwrap();
    let pool = chain.factory.get_pool(&token_in, &token_out).unwrap();
    let mut batch = SorobanVec::new(env);
    for _ in 0..ops {
        batch.push_back(BatchOp::Swap(SwapOp {
            pool: pool.clone(),
            token_in: token_in.clone(),
            amount_in: 100,
            min_out: 0,
            pool_kind: BatchPoolType::Amm,
            zero_for_one: false,
            sqrt_price_limit_x96: 0,
        }));
    }
    env.budget().reset_default();
    let results = router.execute_batch(&chain.trader, &batch, &u64::MAX);
    assert_eq!(results.len(), ops);
}
