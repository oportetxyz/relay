//! Gas-sponsorship policy + usage types.
//!
//! Model A (relay-side, zero-fee): the relay decides whether to sponsor a call,
//! zeroes the user's intent payment when it does, and its funder pays the real
//! on-chain gas. Sponsored spend is recorded post-receipt into `sponsorship_usage`
//! and drives a global circuit breaker + a per-subject rolling quota.

use std::{collections::HashMap, fmt};

use alloy::primitives::{
    Address, B256, ChainId, FixedBytes, U256,
    map::{HashMap as AHashMap, HashSet},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What the per-user rolling quota counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaKey {
    /// Count against the on-chain EOA in the signed intent. The address is
    /// always present, so this needs no JWT — safe default.
    #[default]
    Address,
    /// Count against the Better Auth userId (verified JWT `sub`). Caps a single
    /// human across multiple accounts. Fail-closed when no `sub` is present.
    User,
}

/// Base gas-sponsorship policy. Per-chain overrides live in
/// [`ChainSponsorshipConfig`] and are merged in by [`SponsorshipConfig::resolve`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SponsorshipConfig {
    /// Sponsor every call unconditionally, bypassing every guard, breaker, and
    /// quota. Dev/testnet escape hatch only — never enable on a mainnet relay.
    #[serde(default)]
    pub sponsor_all: bool,
    /// Chain guard: chain ids the relay is allowed to sponsor on (e.g. Base 8453,
    /// Rise 4153, Polygon 137). A call on any chain NOT in this set is never
    /// sponsored. Empty = none (fail-closed).
    #[serde(default)]
    pub sponsored_chains: std::collections::HashSet<ChainId>,
    /// Target guard: contracts our sponsorship is allowed to touch. A sponsored
    /// intent must have EVERY call target either a listed contract or a listed
    /// (contract, selector) pair; anything else is denied (fail-closed). This is
    /// a necessary gate, not a quota exemption — sponsored calls still consume
    /// the per-user quota and count toward the breaker.
    #[serde(default)]
    pub whitelisted_contracts: HashSet<Address>,
    /// Target guard by method: (contract -> allowed 4-byte selectors) additionally
    /// permitted as sponsored targets.
    #[serde(default)]
    pub whitelisted_methods: AHashMap<Address, HashSet<FixedBytes<4>>>,
    /// Global sponsored-spend ceiling per chain over the rolling window (wei).
    /// Reaching it trips the circuit breaker -> deny all further sponsorship.
    pub circuit_breaker_wei: U256,
    /// Primary limiter: per-subject sponsored-spend ceiling over the rolling
    /// window (wei), keyed by `quota_key` (address or verified JWT `sub`).
    pub per_user_wei: U256,
    /// Rolling window for both the breaker and the per-user quota (hours).
    pub window_hours: u64,
    /// Which identity the per-user quota counts against.
    #[serde(default)]
    pub quota_key: QuotaKey,
    /// Relax the target guard for callers with a VERIFIED identity (JWT `sub`
    /// or the dev hatch): their calls may land anywhere, bounded by the
    /// per-user quota and the breaker alone. Anonymous (address-mode) callers
    /// stay on the whitelist. Off by default (fail-closed).
    #[serde(default)]
    pub verified_skips_target_guard: bool,
    /// Per-subject quota overrides (subject -> wei per window). A subject listed
    /// here uses this ceiling instead of `per_user_wei`; subjects not listed use
    /// `per_user_wei`. Used to cap a specific identity — notably the dev
    /// escape-hatch subject — tighter (or looser) than a normal user.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub quota_overrides: HashMap<String, u128>,
    /// Most gas one sponsored transaction may use. A bigger transaction is not sponsored.
    #[serde(default = "default_max_gas_per_tx")]
    pub max_gas_per_tx: u64,
}

const fn default_max_gas_per_tx() -> u64 {
    3_000_000
}

impl Default for SponsorshipConfig {
    fn default() -> Self {
        // Fail-closed: empty whitelist + sponsor_all off sponsors nothing until a
        // policy is configured. Ceilings seed from the reverted merchant-side design.
        Self {
            sponsor_all: false,
            sponsored_chains: std::collections::HashSet::default(),
            whitelisted_contracts: HashSet::default(),
            whitelisted_methods: AHashMap::default(),
            circuit_breaker_wei: U256::from(1_000_000_000_000_000_000u128), // 1 ETH / window
            per_user_wei: U256::from(10_000_000_000_000_000u128),           // 0.01 ETH / window
            window_hours: 24,
            quota_key: QuotaKey::Address,
            verified_skips_target_guard: false,
            quota_overrides: HashMap::new(),
            max_gas_per_tx: default_max_gas_per_tx(),
        }
    }
}

/// Per-chain overrides. Each `Some` field replaces the base value for that chain;
/// `None` falls back to the base [`SponsorshipConfig`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChainSponsorshipConfig {
    /// Override for [`SponsorshipConfig::sponsor_all`].
    #[serde(default)]
    pub sponsor_all: Option<bool>,
    /// Override for [`SponsorshipConfig::whitelisted_contracts`].
    #[serde(default)]
    pub whitelisted_contracts: Option<HashSet<Address>>,
    /// Override for [`SponsorshipConfig::whitelisted_methods`].
    #[serde(default)]
    pub whitelisted_methods: Option<AHashMap<Address, HashSet<FixedBytes<4>>>>,
    /// Override for [`SponsorshipConfig::circuit_breaker_wei`].
    #[serde(default)]
    pub circuit_breaker_wei: Option<U256>,
    /// Override for [`SponsorshipConfig::per_user_wei`].
    #[serde(default)]
    pub per_user_wei: Option<U256>,
    /// Override for [`SponsorshipConfig::window_hours`].
    #[serde(default)]
    pub window_hours: Option<u64>,
    /// Override for [`SponsorshipConfig::quota_key`].
    #[serde(default)]
    pub quota_key: Option<QuotaKey>,
    /// Override for [`SponsorshipConfig::verified_skips_target_guard`].
    #[serde(default)]
    pub verified_skips_target_guard: Option<bool>,
    /// Override for [`SponsorshipConfig::max_gas_per_tx`].
    #[serde(default)]
    pub max_gas_per_tx: Option<u64>,
    /// Merged over [`SponsorshipConfig::quota_overrides`], entry by entry: a cap is
    /// in the chain's own native unit, so one number cannot fit every chain.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub quota_overrides: HashMap<String, u128>,
}

impl SponsorshipConfig {
    /// Resolve the effective policy for a chain: per-chain fields override the
    /// base; unset fields fall back to the base. `quota_overrides` merges instead,
    /// so a subject the chain does not name keeps its global cap.
    pub fn resolve(
        &self,
        chain_id: ChainId,
        overrides: &HashMap<ChainId, ChainSponsorshipConfig>,
    ) -> SponsorshipConfig {
        let Some(o) = overrides.get(&chain_id) else {
            return self.clone();
        };

        let mut quota_overrides = self.quota_overrides.clone();
        quota_overrides.extend(o.quota_overrides.iter().map(|(k, v)| (k.clone(), *v)));

        SponsorshipConfig {
            sponsor_all: o.sponsor_all.unwrap_or(self.sponsor_all),
            // Chain guard is a relay-global allowlist, not per-chain overridable.
            sponsored_chains: self.sponsored_chains.clone(),
            whitelisted_contracts: o
                .whitelisted_contracts
                .clone()
                .unwrap_or_else(|| self.whitelisted_contracts.clone()),
            whitelisted_methods: o
                .whitelisted_methods
                .clone()
                .unwrap_or_else(|| self.whitelisted_methods.clone()),
            circuit_breaker_wei: o.circuit_breaker_wei.unwrap_or(self.circuit_breaker_wei),
            per_user_wei: o.per_user_wei.unwrap_or(self.per_user_wei),
            window_hours: o.window_hours.unwrap_or(self.window_hours),
            quota_key: o.quota_key.unwrap_or(self.quota_key),
            verified_skips_target_guard: o
                .verified_skips_target_guard
                .unwrap_or(self.verified_skips_target_guard),
            quota_overrides,
            max_gas_per_tx: o.max_gas_per_tx.unwrap_or(self.max_gas_per_tx),
        }
    }
}

/// Parameters of `wallet_sponsorshipQuota`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SponsorshipQuotaParameters {
    /// Chain whose policy and ledger to report.
    pub chain_id: ChainId,
    /// The EOA the quota counts against under `quota_key = address`. Ignored
    /// under `quota_key = user`, where the verified JWT `sub` is the subject.
    #[serde(default)]
    pub account: Option<Address>,
}

/// What a user is told about their gas sponsorship on one chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SponsorshipQuotaResponse {
    /// Chain the numbers are for.
    pub chain_id: ChainId,
    /// Whether this chain sponsors gas at all (`sponsor_all` or in the
    /// sponsored set). The caps below are meaningless when false.
    pub sponsored: bool,
    /// Policy escape hatch: everything is sponsored, no quota is enforced.
    pub sponsor_all: bool,
    /// Which identity the per-user cap counts against.
    pub quota_key: QuotaKey,
    /// Length of the rolling window, in hours.
    pub window_hours: u64,
    /// Per-user cap for the window (wei of native gas token).
    pub cap_wei: U256,
    /// Spent inside the window so far.
    pub spent_wei: U256,
    /// `cap_wei - spent_wei`, saturating.
    pub remaining_wei: U256,
    /// Unix seconds when the oldest sponsored transaction in the window ages
    /// out, i.e. when `spent_wei` next drops. It is a rolling window, so this
    /// is a partial release, not a full reset. `None` when nothing was spent.
    pub resets_at: Option<u64>,
    /// Chain-wide circuit breaker, which denies everyone when tripped.
    pub breaker: SponsorshipBreakerStatus,
}

/// Chain-wide breaker state, reported alongside the per-user quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SponsorshipBreakerStatus {
    /// Ceiling for the chain's total sponsored spend in the window.
    pub cap_wei: U256,
    /// Chain-wide spend inside the window.
    pub spent_wei: U256,
    /// `spent_wei >= cap_wei`: nobody is sponsored until spend ages out.
    pub tripped: bool,
}

/// The identity a sponsored transaction counts against: an EOA, or a user's JWT `sub`.
///
/// `Debug` hides the value, since this type sits inside quotes and transactions that get logged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuotaSubject(String);

impl QuotaSubject {
    /// Wraps a subject.
    pub fn new(subject: impl Into<String>) -> Self {
        Self(subject.into())
    }

    /// The subject itself.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for QuotaSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QuotaSubject(..)")
    }
}

/// Percent of the simulated cost an expected cost carries, for a fee that moves before landing.
const EXPECTED_COST_MARGIN_PERCENT: u64 = 125;

/// What a transaction should cost the relay (wei), from its simulated gas and the current fee.
pub fn expected_sponsored_cost(gas: u64, fee_per_gas: u128) -> U256 {
    U256::from(gas).saturating_mul(U256::from(fee_per_gas))
        * U256::from(EXPECTED_COST_MARGIN_PERCENT)
        / U256::from(100)
}

/// One transaction a sponsorship hold covers.
#[derive(Debug, Clone, Copy)]
pub struct SponsoredCost {
    /// Chain the transaction will land on.
    pub chain_id: ChainId,
    /// Gas limit of the transaction.
    pub gas_limit: u64,
    /// Highest fee per gas the signer may pay.
    pub max_fee_per_gas: u128,
    /// What the transaction should cost (wei). A hold never counts more than the maximum.
    pub expected: U256,
}

impl SponsoredCost {
    /// The most the transaction can cost (wei).
    pub fn maximum(&self) -> U256 {
        U256::from(self.gas_limit).saturating_mul(U256::from(self.max_fee_per_gas))
    }
}

/// What one sponsored transaction may still cost, held against the limits it would spend
/// under until it settles.
#[derive(Debug, Clone)]
pub struct SponsorshipHold {
    /// The signed quote's digest, or the transaction id when no quote exists.
    pub digest: B256,
    /// Chain the transaction will land on.
    pub chain_id: ChainId,
    /// The identity the per-user quota counts against.
    pub quota_subject: String,
    /// What the transaction should cost (wei), which the quota counts so that quotes a user
    /// abandons or retries do not spend it several times over.
    pub amount: U256,
    /// The most the transaction can cost (wei), which the breaker counts so that everything
    /// held can land at its maximum without passing it.
    pub max_amount: U256,
    /// When the hold stops counting.
    pub expires_at: DateTime<Utc>,
    /// Whether the transaction has been sent already, so the hold cannot be claimed.
    pub claimed: bool,
    /// The subject's ceiling over the window (wei).
    pub quota_cap: U256,
    /// The chain's ceiling over the window (wei).
    pub breaker_cap: U256,
    /// Rolling window both ceilings apply to (hours).
    pub window_hours: u64,
}

/// One sponsored transaction, recorded post-receipt.
#[derive(Debug, Clone)]
pub struct SponsorshipUsage {
    /// On-chain EOA from the signed intent (always recorded, for audit).
    pub user_address: Address,
    /// The identity the per-user quota counts against (EOA or JWT `sub`).
    pub quota_subject: String,
    /// Chain the sponsored transaction landed on.
    pub chain_id: ChainId,
    /// Transaction hash (unique; guards against double-counting).
    pub tx_hash: String,
    /// Gas units consumed on-chain.
    pub gas_used: U256,
    /// Effective gas price paid (wei).
    pub gas_price: U256,
    /// `gas_used * gas_price`, denormalized for cheap windowed SUM aggregation.
    pub eth_spent: U256,
}
