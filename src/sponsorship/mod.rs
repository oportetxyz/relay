//! Relay-side gas-sponsorship evaluator (Model A, zero-fee).
//!
//! Decides whether a set of calls should be sponsored (whitelist -> circuit
//! breaker -> per-subject rolling quota) and records sponsored spend after a
//! successful send. The relay's own funder pays the on-chain gas; the user's
//! intent payment is zeroed by the caller when this returns `true`.

use std::collections::HashMap;

use alloy::primitives::{Address, ChainId, FixedBytes, U256};
use tracing::{debug, warn};

use crate::{
    error::StorageError,
    storage::{RelayStorage, StorageApi},
    types::{
        Call, ChainSponsorshipConfig, QuotaKey, SponsorshipBreakerStatus, SponsorshipConfig,
        SponsorshipQuotaResponse,
    },
};

/// Evaluates gas-sponsorship policy against the usage ledger.
#[derive(Debug, Clone)]
pub struct SponsorshipEvaluator {
    base: SponsorshipConfig,
    per_chain: HashMap<ChainId, ChainSponsorshipConfig>,
    storage: RelayStorage,
}

impl SponsorshipEvaluator {
    /// Creates an evaluator from the base policy, per-chain overrides, and the
    /// usage-ledger storage.
    pub fn new(
        base: SponsorshipConfig,
        per_chain: HashMap<ChainId, ChainSponsorshipConfig>,
        storage: RelayStorage,
    ) -> Self {
        Self { base, per_chain, storage }
    }

    /// Effective policy for `chain_id` (base merged with per-chain overrides).
    fn config_for(&self, chain_id: ChainId) -> SponsorshipConfig {
        self.base.resolve(chain_id, &self.per_chain)
    }

    /// The identity the per-user quota counts against, per policy. `None` means
    /// deny: `quota_key = user` but no verified userId was supplied.
    fn quota_subject(
        cfg: &SponsorshipConfig,
        eoa: Address,
        user_id: Option<&str>,
    ) -> Option<String> {
        match cfg.quota_key {
            QuotaKey::Address => Some(eoa.to_string()),
            QuotaKey::User => user_id.map(str::to_owned),
        }
    }

    /// The subject a sponsored tx on `chain_id` should be recorded against,
    /// resolved through that chain's policy. Used at send time so the usage
    /// ledger keys the SAME subject the decision counted (address or verified
    /// user). `None` under `quota_key = user` with no verified user — in which
    /// case the tx would not have been sponsored, so nothing is recorded.
    pub fn resolve_quota_subject(
        &self,
        eoa: Address,
        user_id: Option<&str>,
        chain_id: ChainId,
    ) -> Option<String> {
        Self::quota_subject(&self.config_for(chain_id), eoa, user_id)
    }

    /// Target guard: whether every call lands on our sponsored contract set.
    /// A sponsored intent must have EVERY call target either a listed contract
    /// or a listed (contract, selector) pair. Empty call lists, or any call to
    /// an off-set target, are denied (fail-closed) — this is a necessary gate,
    /// not a quota exemption.
    fn targets_allowed(cfg: &SponsorshipConfig, calls: &[Call]) -> bool {
        !calls.is_empty()
            && calls.iter().all(|call| {
                let by_contract = cfg.whitelisted_contracts.contains(&call.to);
                let by_method = cfg
                    .whitelisted_methods
                    .get(&call.to)
                    .is_some_and(|sels| selector(&call.data).is_some_and(|s| sels.contains(&s)));
                by_contract || by_method
            })
    }

    /// Evaluate whether `calls` should be gas-sponsored on `chain_id`.
    ///
    /// Policy order (all fail-closed): `sponsor_all` escape hatch -> chain guard
    /// -> target guard -> global circuit breaker -> per-subject rolling quota.
    /// The auth layer is the only "who" check (any authenticated request is
    /// eligible); the per-user wei quota is the primary limiter.
    ///
    /// `user_id` is the verified Better Auth `sub` (from the JWT / dev hatch),
    /// used only when the policy's `quota_key = user`.
    pub async fn is_sponsored(
        &self,
        eoa: Address,
        user_id: Option<&str>,
        calls: &[Call],
        chain_id: ChainId,
    ) -> Result<bool, StorageError> {
        let cfg = self.config_for(chain_id);

        // Dev/testnet escape hatch: sponsor everything, no guard/breaker/quota.
        if cfg.sponsor_all {
            return Ok(true);
        }

        // Chain guard (fail-closed): only sponsor on chains we explicitly allow.
        if !cfg.sponsored_chains.contains(&chain_id) {
            warn!(%eoa, chain_id, "sponsorship denied: chain not in sponsored set");
            return Ok(false);
        }

        // Target guard (fail-closed): only sponsor calls to our contract set.
        // A verified identity may be let off the whitelist by policy: its
        // spend is still bounded by the per-user quota and the breaker, while
        // an anonymous caller has nothing but the target set to answer to.
        let guard_waived = cfg.verified_skips_target_guard && user_id.is_some();
        if !guard_waived && !Self::targets_allowed(&cfg, calls) {
            warn!(%eoa, chain_id, "sponsorship denied: call targets off our sponsored contract set");
            return Ok(false);
        }

        // Global circuit breaker: once the chain's rolling sponsored spend hits
        // the ceiling, deny all further sponsorship (across all users).
        let global =
            self.storage.global_sponsored_wei_in_window(chain_id, cfg.window_hours).await?;
        if global >= cfg.circuit_breaker_wei {
            warn!(%eoa, chain_id, "sponsorship denied: circuit breaker active");
            return Ok(false);
        }

        // Per-subject rolling quota (primary limiter), keyed by address or the
        // verified user id per policy. A per-subject override (e.g. the dev
        // escape-hatch identity) caps that subject tighter/looser than the
        // global `per_user_wei`.
        let Some(subject) = Self::quota_subject(&cfg, eoa, user_id) else {
            warn!(%eoa, chain_id, "sponsorship denied: quota_key=user but no verified userId");
            return Ok(false);
        };
        let cap =
            cfg.quota_overrides.get(&subject).copied().map(U256::from).unwrap_or(cfg.per_user_wei);
        let spent =
            self.storage.sponsored_wei_in_window(&subject, chain_id, cfg.window_hours).await?;
        if spent >= cap {
            debug!(%eoa, chain_id, "sponsorship denied: per-user quota exhausted");
            return Ok(false);
        }

        Ok(true)
    }

    /// What `wallet_sponsorshipQuota` reports: the same cap, window and ledger
    /// reads `is_sponsored` decides on, so the number a user sees is the number
    /// the relay enforces. `None` when no subject can be resolved (address
    /// mode without an account, user mode without a verified user).
    pub async fn quota_status(
        &self,
        eoa: Option<Address>,
        user_id: Option<&str>,
        chain_id: ChainId,
    ) -> Result<Option<SponsorshipQuotaResponse>, StorageError> {
        let cfg = self.config_for(chain_id);
        let subject = match cfg.quota_key {
            QuotaKey::Address => eoa.map(|a| a.to_string()),
            QuotaKey::User => user_id.map(str::to_owned),
        };
        let Some(subject) = subject else { return Ok(None) };

        let cap =
            cfg.quota_overrides.get(&subject).copied().map(U256::from).unwrap_or(cfg.per_user_wei);
        let spent =
            self.storage.sponsored_wei_in_window(&subject, chain_id, cfg.window_hours).await?;
        let global =
            self.storage.global_sponsored_wei_in_window(chain_id, cfg.window_hours).await?;
        let resets_at = self
            .storage
            .sponsorship_window_start(&subject, chain_id, cfg.window_hours)
            .await?
            .map(|oldest| oldest as u64 + cfg.window_hours * 3600);

        Ok(Some(SponsorshipQuotaResponse {
            chain_id,
            sponsored: cfg.sponsor_all || cfg.sponsored_chains.contains(&chain_id),
            sponsor_all: cfg.sponsor_all,
            quota_key: cfg.quota_key,
            window_hours: cfg.window_hours,
            cap_wei: cap,
            spent_wei: spent,
            remaining_wei: cap.saturating_sub(spent),
            resets_at,
            breaker: SponsorshipBreakerStatus {
                cap_wei: cfg.circuit_breaker_wei,
                spent_wei: global,
                tripped: global >= cfg.circuit_breaker_wei,
            },
        }))
    }
}

/// First 4 bytes of calldata as a selector, if present.
fn selector(data: &[u8]) -> Option<FixedBytes<4>> {
    (data.len() >= 4).then(|| FixedBytes::<4>::from_slice(&data[..4]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{storage::StorageApi, types::SponsorshipUsage};
    use alloy::primitives::{Bytes, address, map::HashSet};

    const CHAIN: ChainId = 84532; // Base Sepolia (a sponsored chain in these tests)
    const OFF_CHAIN: ChainId = 1; // Ethereum mainnet (not in the sponsored set)
    const TARGET: Address = address!("00000000000000000000000000000000000000aa");
    const OFF_TARGET: Address = address!("00000000000000000000000000000000000000bb");
    const EOA: Address = address!("00000000000000000000000000000000000000c0");
    const DEV_SUB: &str = "dev-local";

    /// A policy that gates by verified user id, sponsors only `CHAIN`, and only
    /// permits calls to `TARGET`. 0.01 ETH per-user / 1 ETH global, 24h window.
    fn user_mode_cfg() -> SponsorshipConfig {
        SponsorshipConfig {
            sponsor_all: false,
            sponsored_chains: std::collections::HashSet::from([CHAIN]),
            whitelisted_contracts: HashSet::from_iter([TARGET]),
            circuit_breaker_wei: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
            per_user_wei: U256::from(10_000_000_000_000_000u128),           // 0.01 ETH
            window_hours: 24,
            quota_key: QuotaKey::User,
            ..Default::default()
        }
    }

    fn evaluator(cfg: SponsorshipConfig) -> (SponsorshipEvaluator, RelayStorage) {
        let storage = RelayStorage::in_memory();
        let eval = SponsorshipEvaluator::new(cfg, HashMap::default(), storage.clone());
        (eval, storage)
    }

    fn call_to(to: Address) -> Call {
        Call { to, value: U256::ZERO, data: Bytes::from(vec![0x11, 0x22, 0x33, 0x44]) }
    }

    async fn seed(storage: &RelayStorage, subject: &str, chain_id: ChainId, wei: U256) {
        storage
            .record_sponsorship_usage(SponsorshipUsage {
                user_address: Address::ZERO,
                quota_subject: subject.to_string(),
                chain_id,
                tx_hash: format!("0x{subject}-{wei}"),
                gas_used: U256::from(1u64),
                gas_price: wei,
                eth_spent: wei,
            })
            .await
            .unwrap();
    }

    // A verified identity is let off the whitelist only when the policy says
    // so; an anonymous caller stays on it either way.
    #[tokio::test]
    async fn verified_skips_target_guard_by_policy() {
        let calls = [call_to(OFF_TARGET)];

        let (strict, _) = evaluator(user_mode_cfg());
        assert!(!strict.is_sponsored(EOA, Some(DEV_SUB), &calls, CHAIN).await.unwrap());

        let mut cfg = user_mode_cfg();
        cfg.verified_skips_target_guard = true;
        cfg.quota_key = QuotaKey::Address;
        let (relaxed, _) = evaluator(cfg);
        assert!(relaxed.is_sponsored(EOA, Some(DEV_SUB), &calls, CHAIN).await.unwrap());
        // No verified identity: the whitelist still applies.
        assert!(!relaxed.is_sponsored(EOA, None, &calls, CHAIN).await.unwrap());
        assert!(relaxed.is_sponsored(EOA, None, &[call_to(TARGET)], CHAIN).await.unwrap());
    }

    // (a) quota is tracked under the fixed dev sub, and (b) exceeding the
    // per-user wei quota flips sponsorship from allowed -> refused.
    #[tokio::test]
    async fn per_user_quota_keyed_by_dev_sub() {
        let (eval, storage) = evaluator(user_mode_cfg());
        let calls = [call_to(TARGET)];

        // Fresh dev sub within quota -> sponsored.
        assert!(eval.is_sponsored(EOA, Some(DEV_SUB), &calls, CHAIN).await.unwrap());

        // Record spend under the dev sub that reaches the 0.01 ETH per-user cap.
        seed(&storage, DEV_SUB, CHAIN, U256::from(10_000_000_000_000_000u128)).await;

        // Same dev sub is now refused (quota exhausted)...
        assert!(!eval.is_sponsored(EOA, Some(DEV_SUB), &calls, CHAIN).await.unwrap());
        // ...while a different subject is unaffected -> quota is per-subject.
        assert!(eval.is_sponsored(EOA, Some("other-user"), &calls, CHAIN).await.unwrap());
    }

    // user-mode with no verified sub is fail-closed.
    #[tokio::test]
    async fn user_mode_without_sub_denied() {
        let (eval, _s) = evaluator(user_mode_cfg());
        assert!(!eval.is_sponsored(EOA, None, &[call_to(TARGET)], CHAIN).await.unwrap());
    }

    // (c) the global circuit breaker trips once total sponsored spend across ALL
    // subjects reaches the ceiling, even for a subject with fresh personal quota.
    /// A booking replayed after a restart counts once, as the Postgres unique key makes it.
    #[tokio::test]
    async fn a_repeated_booking_counts_once() {
        let storage = RelayStorage::in_memory();
        seed(&storage, "subject", CHAIN, U256::from(5)).await;
        seed(&storage, "subject", CHAIN, U256::from(5)).await;

        let total = storage.sponsored_wei_in_window("subject", CHAIN, 24).await.unwrap();
        assert_eq!(total, U256::from(5));
    }

    #[tokio::test]
    async fn global_breaker_trips() {
        let (eval, storage) = evaluator(user_mode_cfg());
        let calls = [call_to(TARGET)];

        // Two different subjects each spend 0.6 ETH -> 1.2 ETH global >= 1 ETH cap.
        seed(&storage, "user-a", CHAIN, U256::from(600_000_000_000_000_000u128)).await;
        seed(&storage, "user-b", CHAIN, U256::from(600_000_000_000_000_000u128)).await;

        // A brand-new subject (no personal spend) is still refused by the breaker.
        assert!(!eval.is_sponsored(EOA, Some("fresh-user"), &calls, CHAIN).await.unwrap());
    }

    // (d) target/chain guard: off-set target and off-set chain are both refused,
    // fail-closed. Method-selector whitelisting is also honored.
    #[tokio::test]
    async fn target_and_chain_guards() {
        let (eval, _s) = evaluator(user_mode_cfg());

        // Off-set target contract -> denied.
        assert!(
            !eval.is_sponsored(EOA, Some(DEV_SUB), &[call_to(OFF_TARGET)], CHAIN).await.unwrap()
        );
        // Empty call list -> denied (fail-closed).
        assert!(!eval.is_sponsored(EOA, Some(DEV_SUB), &[], CHAIN).await.unwrap());
        // Allowed target but off-set chain -> denied.
        assert!(
            !eval.is_sponsored(EOA, Some(DEV_SUB), &[call_to(TARGET)], OFF_CHAIN).await.unwrap()
        );
        // A call mixing an allowed and a disallowed target -> denied (all must pass).
        assert!(
            !eval
                .is_sponsored(EOA, Some(DEV_SUB), &[call_to(TARGET), call_to(OFF_TARGET)], CHAIN)
                .await
                .unwrap()
        );
    }

    // Method-selector whitelisting: a target permitted only for a specific
    // selector is sponsored for that selector and denied for others.
    #[tokio::test]
    async fn method_selector_guard() {
        let mut cfg = user_mode_cfg();
        cfg.whitelisted_contracts = HashSet::default();
        let sel = FixedBytes::<4>::from([0x11, 0x22, 0x33, 0x44]);
        cfg.whitelisted_methods.insert(OFF_TARGET, HashSet::from_iter([sel]));
        let (eval, _s) = evaluator(cfg);

        // data starts with the whitelisted selector -> allowed.
        let ok = Call {
            to: OFF_TARGET,
            value: U256::ZERO,
            data: Bytes::from(vec![0x11, 0x22, 0x33, 0x44, 0x99]),
        };
        assert!(eval.is_sponsored(EOA, Some(DEV_SUB), &[ok], CHAIN).await.unwrap());

        // different selector on the same target -> denied.
        let bad = Call {
            to: OFF_TARGET,
            value: U256::ZERO,
            data: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
        };
        assert!(!eval.is_sponsored(EOA, Some(DEV_SUB), &[bad], CHAIN).await.unwrap());
    }

    // sponsor_all bypasses every guard (dev/testnet escape hatch preserved).
    #[tokio::test]
    async fn sponsor_all_bypasses_guards() {
        let mut cfg = user_mode_cfg();
        cfg.sponsor_all = true;
        let (eval, _s) = evaluator(cfg);
        // off-set target, off-set chain, no sub -> still sponsored.
        assert!(eval.is_sponsored(EOA, None, &[call_to(OFF_TARGET)], OFF_CHAIN).await.unwrap());
    }

    // A per-subject override caps that subject TIGHTER than the global
    // per_user_wei: the dev sub is refused at its override cap while a normal
    // sub at the same spend (below the global cap) is still sponsored.
    #[tokio::test]
    async fn override_caps_subject_below_global() {
        let mut cfg = user_mode_cfg(); // global per_user_wei = 0.01 ETH
        // dev-local: tight 0.001 ETH cap.
        cfg.quota_overrides.insert(DEV_SUB.to_string(), 1_000_000_000_000_000u128);
        let (eval, storage) = evaluator(cfg);
        let calls = [call_to(TARGET)];

        // Fresh dev sub within its tight cap -> sponsored.
        assert!(eval.is_sponsored(EOA, Some(DEV_SUB), &calls, CHAIN).await.unwrap());

        // Spend 0.001 ETH under dev-local (== override cap, still << 0.01 global).
        seed(&storage, DEV_SUB, CHAIN, U256::from(1_000_000_000_000_000u128)).await;

        // dev-local now refused by its own override...
        assert!(!eval.is_sponsored(EOA, Some(DEV_SUB), &calls, CHAIN).await.unwrap());
        // ...but a normal sub at the same 0.001 ETH spend is still sponsored,
        // because it uses the 0.01 ETH global cap (no override).
        seed(&storage, "normal-user", CHAIN, U256::from(1_000_000_000_000_000u128)).await;
        assert!(eval.is_sponsored(EOA, Some("normal-user"), &calls, CHAIN).await.unwrap());
    }

    // The dev-hatch identity is NOT a gating bypass: a VerifiedSub(dev) request
    // still flows through the chain guard, target guard, and global breaker.
    #[tokio::test]
    async fn dev_sub_still_fully_gated() {
        let mut cfg = user_mode_cfg();
        cfg.quota_overrides.insert(DEV_SUB.to_string(), 1_000_000_000_000_000u128);
        let (eval, storage) = evaluator(cfg);

        // Chain guard + target guard still reject the dev sub.
        assert!(
            !eval.is_sponsored(EOA, Some(DEV_SUB), &[call_to(OFF_TARGET)], CHAIN).await.unwrap()
        );
        assert!(
            !eval.is_sponsored(EOA, Some(DEV_SUB), &[call_to(TARGET)], OFF_CHAIN).await.unwrap()
        );

        // Global breaker still stops the dev sub, even with fresh personal quota.
        seed(&storage, "user-a", CHAIN, U256::from(600_000_000_000_000_000u128)).await;
        seed(&storage, "user-b", CHAIN, U256::from(600_000_000_000_000_000u128)).await;
        assert!(!eval.is_sponsored(EOA, Some(DEV_SUB), &[call_to(TARGET)], CHAIN).await.unwrap());
    }

    // The status endpoint reports the same cap/spend the decision path
    // enforces, plus when the oldest spend ages out of the rolling window.
    #[tokio::test]
    async fn quota_status_reports_cap_spend_and_reset() {
        let cfg = user_mode_cfg(); // per_user_wei = 0.01 ETH, breaker = 1 ETH
        let (eval, storage) = evaluator(cfg);
        let spent = U256::from(4_000_000_000_000_000u128); // 0.004 ETH
        seed(&storage, DEV_SUB, CHAIN, spent).await;
        let before = chrono::Utc::now().timestamp() as u64;

        let status = eval.quota_status(None, Some(DEV_SUB), CHAIN).await.unwrap().unwrap();

        assert!(status.sponsored && !status.sponsor_all);
        assert_eq!(status.quota_key, QuotaKey::User);
        assert_eq!(status.cap_wei, U256::from(10_000_000_000_000_000u128));
        assert_eq!(status.spent_wei, spent);
        assert_eq!(status.remaining_wei, U256::from(6_000_000_000_000_000u128));
        assert_eq!(status.breaker.spent_wei, spent);
        assert!(!status.breaker.tripped);
        let resets_at = status.resets_at.expect("something was spent");
        let window = status.window_hours * 3600;
        assert!(resets_at + 5 >= before + window && resets_at <= before + window + 5);

        // Nothing spent: no reset time, full cap remaining.
        let fresh = eval.quota_status(None, Some("someone-else"), CHAIN).await.unwrap().unwrap();
        assert_eq!(fresh.resets_at, None);
        assert_eq!(fresh.remaining_wei, fresh.cap_wei);

        // User mode with no verified user has no subject to report on.
        assert!(eval.quota_status(Some(EOA), None, CHAIN).await.unwrap().is_none());
    }
}
