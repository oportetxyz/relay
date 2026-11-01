//! The sponsorship ledger records what the relay paid, whatever the transaction did on chain.

use crate::e2e::{
    AuthKind, await_calls_status,
    cases::upgrade_account_eagerly,
    environment::{Environment, mint_erc20s},
    send_prepared_calls,
};
use alloy::{
    primitives::{Address, B256, Bytes, U256, bytes},
    providers::{Provider, ext::AnvilApi},
    rpc::types::TransactionReceipt,
};
use chrono::{TimeDelta, Utc};
use futures_util::future::join_all;
use http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use relay::{
    config::AuthConfig,
    rpc::RelayApiClient,
    signers::Eip712PayLoadSigner,
    storage::StorageApi,
    types::{
        Call, IERC20, KeyType, KeyWith712Signer,
        OrchestratorContract::IntentExecuted,
        QuotaKey, SponsorshipConfig, SponsorshipHold, SponsorshipQuotaParameters, SponsorshipUsage,
        rpc::{
            BundleId, Meta, PrepareCallsCapabilities, PrepareCallsParameters, PrepareCallsResponse,
        },
    },
};
use sqlx::{PgPool, raw_sql};
use std::time::{Duration, SystemTime};

/// `PUSH1 0 PUSH1 0 REVERT`
pub(super) const REVERTS: Bytes = bytes!("60006000fd");

/// What the intent does once it reaches the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The intent executes.
    Succeeds,
    /// The transaction lands, but the intent's own call reverts.
    CallReverts,
    /// The transaction lands and emits no `IntentExecuted`.
    NoEvent,
    /// The transaction itself reverts.
    TransactionReverts,
}

impl Outcome {
    /// Makes the outcome true on chain, after the relay has simulated and broadcast the
    /// transaction.
    async fn apply(self, env: &Environment) -> eyre::Result<()> {
        let provider = env.provider();
        match self {
            Self::Succeeds => {}
            Self::CallReverts => provider.anvil_set_code(env.erc20, REVERTS).await?,
            Self::NoEvent => provider.anvil_set_code(env.orchestrator, Bytes::new()).await?,
            Self::TransactionReverts => provider.anvil_set_code(env.orchestrator, REVERTS).await?,
        }
        Ok(())
    }

    /// Fails unless `receipt` shows this outcome, so a case cannot pass on the wrong path.
    fn assert_matches(self, receipt: &TransactionReceipt) {
        let event = IntentExecuted::try_from_receipt(receipt);
        match self {
            Self::Succeeds => {
                assert!(receipt.status());
                assert!(!event.expect("an IntentExecuted").has_error());
            }
            Self::CallReverts => {
                assert!(receipt.status());
                assert!(event.expect("an IntentExecuted").has_error());
            }
            Self::NoEvent => {
                assert!(receipt.status());
                assert!(event.is_none());
            }
            Self::TransactionReverts => assert!(!receipt.status()),
        }
    }
}

/// Waits until the relay has broadcast a transaction into anvil's pool.
pub(super) async fn wait_for_mempool(env: &Environment) -> eyre::Result<()> {
    for _ in 0..100 {
        let status: serde_json::Value =
            env.provider().raw_request("txpool_status".into(), ()).await?;
        if status["pending"].as_str().is_some_and(|pending| pending != "0x0") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eyre::bail!("no transaction reached the pool")
}

/// The only transaction in the latest block, and what it cost in wei.
pub(super) async fn latest_transaction(
    env: &Environment,
) -> eyre::Result<(TransactionReceipt, U256)> {
    let receipts = env.provider().get_block_receipts(Default::default()).await?;
    let [receipt] = receipts.unwrap_or_default().try_into().map_err(|receipts: Vec<_>| {
        eyre::eyre!("expected one transaction in the block, found {}", receipts.len())
    })?;
    let spent = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
    Ok((receipt, spent))
}

/// The ledger's total for `subject`.
async fn booked_for(env: &Environment, subject: &str) -> eyre::Result<U256> {
    Ok(env.relay_handle.storage.sponsored_wei_in_window(subject, env.chain_id(), 24).await?)
}

/// The ledger's total for the test account.
pub(super) async fn booked(env: &Environment) -> eyre::Result<U256> {
    booked_for(env, &env.eoa.address().to_string()).await
}

/// Fails unless the ledger holds exactly `expected`. A final status means the booking is done.
pub(super) async fn assert_booked(env: &Environment, expected: U256) -> eyre::Result<()> {
    assert_eq!(booked(env).await?, expected);
    Ok(())
}

/// Prepares the test account's intent.
async fn prepare(env: &Environment, key: &KeyWith712Signer) -> eyre::Result<PrepareCallsResponse> {
    prepare_through(&env.relay_endpoint, env, key).await
}

/// Prepares the test account's intent through `client`.
async fn prepare_through(
    client: &HttpClient,
    env: &Environment,
    key: &KeyWith712Signer,
) -> eyre::Result<PrepareCallsResponse> {
    prepare_with_nonce(client, env, key, None).await
}

/// Prepares the test account's intent at `nonce`.
async fn prepare_with_nonce(
    client: &HttpClient,
    env: &Environment,
    key: &KeyWith712Signer,
    nonce: Option<U256>,
) -> eyre::Result<PrepareCallsResponse> {
    Ok(client
        .prepare_calls(PrepareCallsParameters {
            from: Some(env.eoa.address()),
            calls: vec![Call::transfer(env.erc20, Address::random(), U256::from(1))],
            chain_id: env.chain_id(),
            key: Some(key.to_call_key()),
            capabilities: PrepareCallsCapabilities {
                meta: Meta { fee_payer: None, fee_token: Some(env.fee_token), nonce },
                ..Default::default()
            },
            ..Default::default()
        })
        .await?)
}

/// What the quote charges the user. Zero means the relay sponsors it.
fn charged(response: &PrepareCallsResponse) -> U256 {
    let quotes = response.context.quote().expect("a quote").ty();
    quotes.quotes.first().expect("one quote").intent.total_payment_max_amount()
}

/// The most the quote can cost the relay.
fn max_cost(response: &PrepareCallsResponse) -> U256 {
    let quote = response.context.quote().expect("a quote").ty().quotes.first().expect("one quote");
    U256::from(quote.tx_gas) * U256::from(quote.native_fee_estimate.max_fee_per_gas)
}

/// Sends an intent, makes `outcome` true on chain, and returns what the relay paid.
async fn land(env: &Environment, key: &KeyWith712Signer, outcome: Outcome) -> eyre::Result<U256> {
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;
    let response = prepare(env, key).await?;
    land_prepared(env, key, response, outcome).await
}

/// Signs and sends `response`, makes `outcome` true on chain, and returns what the relay paid.
async fn land_prepared(
    env: &Environment,
    key: &KeyWith712Signer,
    response: PrepareCallsResponse,
    outcome: Outcome,
) -> eyre::Result<U256> {
    let signature = key.sign_payload_hash(response.digest).await?;

    env.disable_mining().await;
    let bundle_id = send_prepared_calls(env, key, signature, response.context).await?;
    wait_for_mempool(env).await?;
    outcome.apply(env).await?;
    env.mine_block().await;

    let (receipt, spent) = latest_transaction(env).await?;
    outcome.assert_matches(&receipt);
    env.enable_mining().await;
    await_calls_status(env, bundle_id).await?;
    Ok(spent)
}

/// Upgrades the test account, with `sponsorship` applied on the relay.
async fn account(
    env: &mut Environment,
    sponsorship: SponsorshipConfig,
) -> eyre::Result<KeyWith712Signer> {
    let key = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_eagerly(env, &[key.to_authorized()], &key, AuthKind::Auth).await?;

    let mut config = env.config.clone();
    config.sponsorship = sponsorship;
    env.restart_relay(config).await?;
    Ok(key)
}

async fn sponsored_spend_is_booked(outcome: Outcome) -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsor_all = SponsorshipConfig { sponsor_all: true, ..Default::default() };
    let key = account(&mut env, sponsor_all).await?;

    let spent = land(&env, &key, outcome).await?;
    assert!(!spent.is_zero());
    assert_booked(&env, spent).await
}

#[tokio::test(flavor = "multi_thread")]
async fn books_a_sponsored_intent_that_succeeds() -> eyre::Result<()> {
    sponsored_spend_is_booked(Outcome::Succeeds).await
}

#[tokio::test(flavor = "multi_thread")]
async fn books_a_sponsored_intent_whose_call_reverts() -> eyre::Result<()> {
    sponsored_spend_is_booked(Outcome::CallReverts).await
}

#[tokio::test(flavor = "multi_thread")]
async fn books_a_sponsored_intent_with_no_event() -> eyre::Result<()> {
    sponsored_spend_is_booked(Outcome::NoEvent).await
}

#[tokio::test(flavor = "multi_thread")]
async fn books_a_sponsored_transaction_that_reverts() -> eyre::Result<()> {
    sponsored_spend_is_booked(Outcome::TransactionReverts).await
}

/// A paying user is not sponsored, so a failed intent of theirs stays off the ledger.
#[tokio::test(flavor = "multi_thread")]
async fn does_not_book_a_paying_intent_that_fails() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let key = account(&mut env, Default::default()).await?;

    land(&env, &key, Outcome::CallReverts).await?;

    assert_booked(&env, U256::ZERO).await
}

/// The point of the ledger: spend on a failed intent counts against the user's next one.
#[tokio::test(flavor = "multi_thread")]
async fn failed_spend_counts_against_the_quota() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;

    let spent = land(&env, &key, Outcome::CallReverts).await?;
    assert!(!spent.is_zero());

    let hold = |quota_cap| SponsorshipHold {
        digest: B256::random(),
        chain_id: env.chain_id(),
        quota_subject: env.eoa.address().to_string(),
        amount: U256::from(10),
        max_amount: U256::from(10),
        expires_at: Utc::now() + TimeDelta::minutes(1),
        claimed: false,
        quota_cap,
        breaker_cap: U256::MAX,
        window_hours: 24,
    };
    let storage = &env.relay_handle.storage;
    assert!(!storage.reserve_sponsorship(vec![hold(spent + U256::from(9))]).await?);
    assert!(storage.reserve_sponsorship(vec![hold(spent + U256::from(10))]).await?);

    Ok(())
}

/// Makes every write to the sponsorship ledger fail, as a database fault would, until
/// [`Self::allow`]. Postgres only: the in-memory ledger has no way to fail.
struct RefusedLedger(PgPool);

impl RefusedLedger {
    /// Starts refusing, or `None` when the relay is not on Postgres.
    async fn start(env: &Environment) -> eyre::Result<Option<Self>> {
        let Some(url) = &env.config.database_url else { return Ok(None) };
        let pool = PgPool::connect(url).await?;
        raw_sql(
            "CREATE FUNCTION refuse_booking() RETURNS trigger LANGUAGE plpgsql AS
                $$ BEGIN RAISE EXCEPTION 'ledger refused'; END $$;
             CREATE TRIGGER refuse_booking BEFORE INSERT ON sponsorship_usage
                FOR EACH ROW EXECUTE FUNCTION refuse_booking();",
        )
        .execute(&pool)
        .await?;
        Ok(Some(Self(pool)))
    }

    async fn allow(&self) -> eyre::Result<()> {
        raw_sql("DROP TRIGGER refuse_booking ON sponsorship_usage").execute(&self.0).await?;
        Ok(())
    }
}

/// A sponsored intent landed while the ledger refuses it.
struct RefusedLanding {
    ledger: RefusedLedger,
    bundle_id: BundleId,
    signer: Address,
    spent: U256,
}

impl RefusedLanding {
    /// Lands one on a fresh account, or `None` when the relay is not on Postgres.
    async fn land() -> eyre::Result<Option<(Environment, Self)>> {
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(None);
        }
        let mut env = Environment::setup().await?;
        let sponsor_all = SponsorshipConfig { sponsor_all: true, ..Default::default() };
        let key = account(&mut env, sponsor_all).await?;
        mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

        let response = prepare(&env, &key).await?;
        let signature = key.sign_payload_hash(response.digest).await?;
        let ledger = RefusedLedger::start(&env).await?.expect("a relay on Postgres");

        env.disable_mining().await;
        let bundle_id = send_prepared_calls(&env, &key, signature, response.context).await?;
        wait_for_mempool(&env).await?;
        env.mine_block().await;
        let (receipt, spent) = latest_transaction(&env).await?;
        env.enable_mining().await;

        let landing = Self { ledger, bundle_id, signer: receipt.from, spent };
        Ok(Some((env, landing)))
    }

    /// How many transactions the signer still holds as pending.
    async fn pending(&self, env: &Environment) -> eyre::Result<usize> {
        let storage = &env.relay_handle.storage;
        Ok(storage.read_pending_transactions(self.signer, env.chain_id()).await?.len())
    }

    /// Fails unless the failed write held everything back.
    async fn assert_held(&self, env: &Environment) -> eyre::Result<()> {
        assert_booked(env, U256::ZERO).await?;
        assert_eq!(self.pending(env).await?, 1, "the pending row is the only way back");
        let status = env.relay_endpoint.get_calls_status(self.bundle_id).await.ok();
        assert!(status.is_none_or(|status| status.receipts.is_empty()), "confirmed unbooked");
        Ok(())
    }

    /// Fails unless the landing ended booked, confirmed and off the pending rows.
    async fn assert_finished(&self, env: &Environment) -> eyre::Result<()> {
        await_calls_status(env, self.bundle_id).await?;
        assert_booked(env, self.spent).await?;
        assert_eq!(self.pending(env).await?, 0);
        Ok(())
    }
}

/// The signer's count of confirmations it gave up on, summed over its signers.
fn write_failures(env: &Environment) -> u64 {
    let chain_id = format!("chain_id=\"{}\"", env.chain_id());
    env.relay_handle
        .metrics
        .render()
        .lines()
        .filter(|line| {
            line.starts_with("signer_confirmation_write_failures{") && line.contains(&chain_id)
        })
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

/// A write that fails once holds the confirmation back, and the next attempt lands it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_booking_is_retried() -> eyre::Result<()> {
    let Some((env, landing)) = RefusedLanding::land().await? else { return Ok(()) };

    tokio::time::sleep(Duration::from_secs(1)).await;
    landing.assert_held(&env).await?;

    landing.ledger.allow().await?;
    landing.assert_finished(&env).await
}

/// Past the retries the row stays and nothing books it, until a restart replays it.
#[tokio::test(flavor = "multi_thread")]
async fn a_booking_out_of_retries_is_replayed_after_a_restart() -> eyre::Result<()> {
    let Some((mut env, landing)) = RefusedLanding::land().await? else { return Ok(()) };

    let before = write_failures(&env);
    for _ in 0..300 {
        if write_failures(&env) > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(write_failures(&env), before + 1);
    landing.assert_held(&env).await?;

    landing.ledger.allow().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    landing.assert_held(&env).await?;

    env.restart_relay(env.config.clone()).await?;
    for _ in 0..100 {
        if landing.pending(&env).await? == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    landing.assert_finished(&env).await
}

/// A replayed receipt counts once in the ledger the relay runs on.
#[tokio::test(flavor = "multi_thread")]
async fn the_ledger_counts_a_repeated_hash_once() -> eyre::Result<()> {
    let env = Environment::setup().await?;
    let spent = U256::from(21_000);
    let usage = SponsorshipUsage {
        user_address: env.eoa.address(),
        quota_subject: env.eoa.address().to_string(),
        chain_id: env.chain_id(),
        tx_hash: B256::repeat_byte(1).to_string(),
        gas_used: spent,
        gas_price: U256::from(1),
        eth_spent: spent,
    };

    let storage = &env.relay_handle.storage;
    storage.record_sponsorship_usage(usage.clone()).await?;
    storage.record_sponsorship_usage(usage).await?;

    assert_booked(&env, spent).await
}

/// The subject the quote was prepared for is the one booked, so sending with no token changes
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn books_the_subject_bound_at_prepare() -> eyre::Result<()> {
    const DEV_KEY: &str = "e2e-dev-key";
    const SUBJECT: &str = "dev-local";

    let mut env = Environment::setup().await?;
    let sponsorship = SponsorshipConfig {
        sponsored_chains: [env.chain_id()].into(),
        whitelisted_contracts: std::iter::once(env.erc20).collect(),
        per_user_wei: U256::from(u128::MAX),
        circuit_breaker_wei: U256::MAX,
        window_hours: 24,
        quota_key: QuotaKey::User,
        quota_overrides: [(SUBJECT.to_string(), u128::MAX)].into(),
        ..Default::default()
    };
    env.config.auth = Some(AuthConfig {
        jwks_url: "http://127.0.0.1:1/jwks".to_string(),
        dev_api_key: Some(DEV_KEY.to_string()),
        dev_subject: None,
    });
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let with_token = HttpClientBuilder::default()
        .set_headers(HeaderMap::from_iter([(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {DEV_KEY}"))?,
        )]))
        .build(env.relay_handle.http_url())?;
    let response = prepare_through(&with_token, &env, &key).await?;
    assert!(charged(&response).is_zero());

    let before = booked_for(&env, SUBJECT).await?;
    let spent = land_prepared(&env, &key, response, Outcome::Succeeds).await?;

    assert_eq!(booked_for(&env, SUBJECT).await?, before + spent);
    assert_booked(&env, U256::ZERO).await
}

/// What one quote holds against the quota, and the most it can cost.
#[derive(Clone, Copy)]
struct Costs {
    expected: U256,
    maximum: U256,
}

/// Prepares `count` quotes before any is sent, sends them all, and returns how many the relay
/// sponsored. `limits` receives what one quote costs and returns the policy to run under.
async fn sponsor_inside_one_ttl(
    env: &mut Environment,
    count: u64,
    limits: impl FnOnce(Costs) -> SponsorshipConfig,
) -> eyre::Result<usize> {
    let unlimited = limited(env, U256::MAX, U256::MAX);
    let key = account(env, unlimited).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;
    let probe = prepare(env, &key).await?;
    let costs = Costs { expected: held(env).await?, maximum: max_cost(&probe) };
    let digest = probe.context.quote().expect("a quote").ty().digest();
    env.relay_handle.storage.release_sponsorship(digest, env.chain_id()).await?;
    let mut config = env.config.clone();
    config.sponsorship = limits(costs);
    env.restart_relay(config).await?;

    let mut prepared = Vec::new();
    for sequence in 1..=count {
        let nonce = Some(U256::from(sequence) << 64);
        prepared.push(prepare_with_nonce(&env.relay_endpoint, env, &key, nonce).await?);
    }
    let sponsored = prepared.iter().filter(|response| charged(response).is_zero()).count();

    for response in prepared {
        let signature = key.sign_payload_hash(response.digest).await?;
        let bundle_id = send_prepared_calls(env, &key, signature, response.context).await?;
        await_calls_status(env, bundle_id).await?;
    }
    Ok(sponsored)
}

/// A policy on the test chain with the two limits it is given.
fn limited(env: &Environment, per_user_wei: U256, circuit_breaker_wei: U256) -> SponsorshipConfig {
    SponsorshipConfig {
        sponsored_chains: [env.chain_id()].into(),
        whitelisted_contracts: std::iter::once(env.erc20).collect(),
        per_user_wei,
        circuit_breaker_wei,
        window_hours: 24,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn quotes_prepared_inside_one_ttl_stop_at_the_quota() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let chain = env.chain_id();
    let erc20 = env.erc20;
    let sponsored = sponsor_inside_one_ttl(&mut env, 5, |cost| SponsorshipConfig {
        sponsored_chains: [chain].into(),
        whitelisted_contracts: std::iter::once(erc20).collect(),
        per_user_wei: cost.expected * U256::from(5) / U256::from(2),
        circuit_breaker_wei: U256::MAX,
        window_hours: 24,
        ..Default::default()
    })
    .await?;

    assert_eq!(sponsored, 2);
    Ok(())
}

// The reason a hold is not the maximum: a quota one maximum wide still takes several quotes,
// as when a user abandons a prompt and tries again.
#[tokio::test(flavor = "multi_thread")]
async fn quotes_inside_one_ttl_fit_a_quota_narrower_than_their_maximum() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let chain = env.chain_id();
    let erc20 = env.erc20;
    let mut seen = None;
    let sponsored = sponsor_inside_one_ttl(&mut env, 2, |cost| {
        seen = Some(cost);
        SponsorshipConfig {
            sponsored_chains: [chain].into(),
            whitelisted_contracts: std::iter::once(erc20).collect(),
            per_user_wei: cost.expected * U256::from(2),
            circuit_breaker_wei: U256::MAX,
            window_hours: 24,
            ..Default::default()
        }
    })
    .await?;

    let cost = seen.expect("the limits were read");
    assert!(cost.expected * U256::from(2) < cost.maximum);
    assert_eq!(sponsored, 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quotes_prepared_inside_one_ttl_stop_at_the_breaker() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let chain = env.chain_id();
    let erc20 = env.erc20;
    let sponsored = sponsor_inside_one_ttl(&mut env, 5, |cost| SponsorshipConfig {
        sponsored_chains: [chain].into(),
        whitelisted_contracts: std::iter::once(erc20).collect(),
        per_user_wei: U256::MAX,
        circuit_breaker_wei: cost.maximum * U256::from(5) / U256::from(2),
        window_hours: 24,
        ..Default::default()
    })
    .await?;

    assert_eq!(sponsored, 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sponsored_quote_is_sent_once() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(&env, &key).await?;
    let signature = key.sign_payload_hash(response.digest).await?;
    env.disable_mining().await;

    let storage = &env.relay_handle.storage;
    let first =
        send_prepared_calls(&env, &key, signature.clone(), response.context.clone()).await?;
    assert_eq!(storage.get_bundle_transactions(first).await?.len(), 1);

    // The answer got lost, so the transport sends it again: same bundle, nothing new sent.
    let again = send_prepared_calls(&env, &key, signature.clone(), response.context.clone());
    assert_eq!(again.await?, first);
    assert_eq!(storage.get_bundle_transactions(first).await?.len(), 1);
    env.enable_mining().await;
    await_calls_status(&env, first).await?;
    assert_eq!(held(&env).await?, U256::ZERO);

    // Still true once it landed and its hold is gone.
    assert_eq!(send_prepared_calls(&env, &key, signature, response.context).await?, first);
    assert_eq!(storage.get_bundle_transactions(first).await?.len(), 1);
    Ok(())
}

// A resend is answered only when something went out. With no hold left to claim and no bundle
// to point at, a sponsored quote stays refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_quote_with_no_hold_and_no_bundle_is_refused() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(&env, &key).await?;
    let digest = response.context.quote().expect("a quote").ty().digest();
    env.relay_handle.storage.release_sponsorship(digest, env.chain_id()).await?;

    let signature = key.sign_payload_hash(response.digest).await?;
    assert!(send_prepared_calls(&env, &key, signature, response.context).await.is_err());
    Ok(())
}

// A user who confirms in the last moments of the TTL gets their send, like any other.
#[tokio::test(flavor = "multi_thread")]
async fn a_quote_sent_at_the_end_of_its_ttl_lands() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    env.config = env.config.clone().with_quote_ttl(Duration::from_secs(3));
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(&env, &key).await?;
    assert!(charged(&response).is_zero());
    let ttl = response.context.quote().expect("a quote").ty().ttl;
    let left = ttl.duration_since(SystemTime::now())?;
    tokio::time::sleep(left - Duration::from_millis(150)).await;

    let signature = key.sign_payload_hash(response.digest).await?;
    let bundle = send_prepared_calls(&env, &key, signature, response.context).await?;
    await_calls_status(&env, bundle).await?;
    Ok(())
}

// A send past the quote check has already been let through, so its hold has to be there
// however late it was and whatever the clock of the database says.
#[tokio::test(flavor = "multi_thread")]
async fn a_hold_outlives_its_quote() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    env.config = env.config.clone().with_quote_ttl(Duration::from_secs(1));
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(&env, &key).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let digest = response.context.quote().expect("a quote").ty().digest();
    let until = Utc::now() + TimeDelta::minutes(1);
    assert!(env.relay_handle.storage.claim_sponsorship(digest, until).await?);
    Ok(())
}

// What a transport retry does after a send that failed: the same signed quote goes again.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_that_failed_can_go_again() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(&env, &key).await?;
    let signature = key.sign_payload_hash(response.digest).await?;

    // An account that lost its delegation is refused after the claim, before anything is sent.
    let eoa = env.eoa.address();
    let code = env.provider().get_code_at(eoa).await?;
    env.provider().anvil_set_code(eoa, Bytes::new()).await?;
    let refused =
        send_prepared_calls(&env, &key, signature.clone(), response.context.clone()).await;
    assert!(refused.is_err());
    assert!(!held(&env).await?.is_zero());
    env.provider().anvil_set_code(eoa, code).await?;

    let bundle = send_prepared_calls(&env, &key, signature, response.context).await?;
    await_calls_status(&env, bundle).await?;
    Ok(())
}

/// The live holds of the test account.
async fn held(env: &Environment) -> eyre::Result<U256> {
    let subject = env.eoa.address().to_string();
    Ok(env.relay_handle.storage.held_sponsorship_wei(&subject, env.chain_id()).await?)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hold_is_replaced_by_the_real_spend_at_landing() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(&env, &key).await?;
    assert!(charged(&response).is_zero());
    let expected = held(&env).await?;
    assert!(!expected.is_zero() && expected < max_cost(&response));

    let spent = land_prepared(&env, &key, response, Outcome::Succeeds).await?;
    assert_eq!(held(&env).await?, U256::ZERO);
    assert!(spent <= expected, "landed at {spent}, expected at most {expected}");
    println!("sponsored cost: spent {spent}, expected {expected}");
    assert_booked(&env, spent).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_that_never_lands_gives_its_hold_back() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    // Accepted at send, and dropped by the signer's validation.
    let response = prepare(&env, &key).await?;
    let stranger = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let signature = stranger.sign_payload_hash(response.digest).await?;
    let bundle_id = send_prepared_calls(&env, &stranger, signature, response.context).await?;
    let _ = await_calls_status(&env, bundle_id).await;

    assert_eq!(held(&env).await?, U256::ZERO);
    assert_booked(&env, U256::ZERO).await
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsent_quote_gives_its_hold_back_when_it_expires() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    env.config = env.config.clone().with_quote_ttl(Duration::from_secs(2));
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    prepare(&env, &key).await?;
    let cost = held(&env).await?;
    assert!(!cost.is_zero());

    // Past the quote's two seconds and the margin the hold keeps after it.
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(held(&env).await?, U256::ZERO);

    let mut config = env.config.clone();
    config.sponsorship = limited(&env, cost * U256::from(2), U256::MAX);
    env.restart_relay(config).await?;
    assert!(charged(&prepare(&env, &key).await?).is_zero());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quote_over_the_gas_cap_is_not_sponsored() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship =
        SponsorshipConfig { max_gas_per_tx: 21_000, ..limited(&env, U256::MAX, U256::MAX) };
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    assert!(!charged(&prepare(&env, &key).await?).is_zero());
    assert_eq!(held(&env).await?, U256::ZERO);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_holds_cannot_share_the_same_room() -> eyre::Result<()> {
    let env = Environment::setup().await?;
    let storage = &env.relay_handle.storage;
    let hold = |quota_cap, breaker_cap| SponsorshipHold {
        digest: B256::random(),
        chain_id: env.chain_id(),
        quota_subject: format!("subject-{}", B256::random()),
        amount: U256::from(10),
        max_amount: U256::from(10),
        expires_at: Utc::now() + TimeDelta::minutes(1),
        claimed: false,
        quota_cap,
        breaker_cap,
        window_hours: 24,
    };

    let one_subject = hold(U256::from(50), U256::MAX);
    let accepted = join_all((0..20).map(|_| {
        storage.reserve_sponsorship(vec![SponsorshipHold {
            digest: B256::random(),
            ..one_subject.clone()
        }])
    }))
    .await;
    let accepted = accepted.into_iter().collect::<Result<Vec<_>, _>>()?;
    assert_eq!(accepted.iter().filter(|fits| **fits).count(), 5);

    let accepted = join_all(
        (0..20).map(|_| storage.reserve_sponsorship(vec![hold(U256::MAX, U256::from(150))])),
    )
    .await;
    let accepted = accepted.into_iter().collect::<Result<Vec<_>, _>>()?;
    assert_eq!(accepted.iter().filter(|fits| **fits).count(), 10);
    Ok(())
}

// A hold that counts little against the quota still takes its whole maximum from the breaker,
// so holds accepted together cannot land past it.
#[tokio::test(flavor = "multi_thread")]
async fn the_breaker_counts_the_most_a_hold_can_cost() -> eyre::Result<()> {
    let env = Environment::setup().await?;
    let storage = &env.relay_handle.storage;
    let subject = format!("subject-{}", B256::random());
    let hold = |quota_cap, breaker_cap| {
        vec![SponsorshipHold {
            digest: B256::random(),
            chain_id: env.chain_id(),
            quota_subject: subject.clone(),
            amount: U256::from(1),
            max_amount: U256::from(100),
            expires_at: Utc::now() + TimeDelta::minutes(1),
            claimed: false,
            quota_cap,
            breaker_cap,
            window_hours: 24,
        }]
    };

    assert!(storage.reserve_sponsorship(hold(U256::from(3), U256::from(250))).await?);
    assert!(storage.reserve_sponsorship(hold(U256::from(3), U256::from(250))).await?);
    assert!(!storage.reserve_sponsorship(hold(U256::from(3), U256::from(250))).await?);

    assert!(storage.reserve_sponsorship(hold(U256::from(3), U256::MAX)).await?);
    assert!(!storage.reserve_sponsorship(hold(U256::from(3), U256::MAX)).await?);

    // One that does not fit takes the ones before it down with it.
    let room = storage.held_sponsorship_wei(&subject, env.chain_id()).await?;
    let both = [hold(U256::from(100), U256::MAX), hold(U256::from(3), U256::MAX)].concat();
    assert!(!storage.reserve_sponsorship(both).await?);
    assert_eq!(storage.held_sponsorship_wei(&subject, env.chain_id()).await?, room);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quote_the_account_cannot_fund_holds_nothing() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    let key = account(&mut env, sponsorship).await?;

    let balance = IERC20::IERC20Instance::new(env.erc20, env.provider())
        .balanceOf(env.eoa.address())
        .call()
        .await?;
    let response = env
        .relay_endpoint
        .prepare_calls(PrepareCallsParameters {
            from: Some(env.eoa.address()),
            calls: vec![Call::transfer(env.erc20, Address::random(), balance + U256::from(1))],
            chain_id: env.chain_id(),
            capabilities: PrepareCallsCapabilities {
                meta: Meta { fee_payer: None, fee_token: Some(env.fee_token), nonce: None },
                authorize_keys: vec![],
                revoke_keys: vec![],
                pre_calls: vec![],
                pre_call: false,
                required_funds: vec![],
            },
            state_overrides: Default::default(),
            balance_overrides: Default::default(),
            key: Some(key.to_call_key()),
        })
        .await?;

    assert!(response.context.quote().expect("a quote").ty().quotes[0].has_deficits());
    assert_eq!(held(&env).await?, U256::ZERO);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quote_for_an_unknown_account_holds_nothing() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let sponsorship = limited(&env, U256::MAX, U256::MAX);
    account(&mut env, sponsorship).await?;

    let eoa = Address::random();
    mint_erc20s(&[env.erc20, env.fee_token], &[eoa], env.provider()).await?;
    let response = env
        .relay_endpoint
        .prepare_calls(PrepareCallsParameters {
            from: Some(eoa),
            calls: vec![Call::transfer(env.erc20, Address::random(), U256::from(1))],
            chain_id: env.chain_id(),
            capabilities: PrepareCallsCapabilities {
                meta: Meta { fee_payer: None, fee_token: Some(env.fee_token), nonce: None },
                authorize_keys: vec![],
                revoke_keys: vec![],
                pre_calls: vec![],
                pre_call: false,
                required_funds: vec![],
            },
            state_overrides: Default::default(),
            balance_overrides: Default::default(),
            key: None,
        })
        .await?;

    // why: funded, so the only thing keeping this quote from holding is the unknown account.
    assert!(!response.context.quote().expect("a quote").ty().quotes[0].has_deficits());
    let holds =
        env.relay_handle.storage.held_sponsorship_wei(&eoa.to_string(), env.chain_id()).await?;
    assert_eq!(holds, U256::ZERO);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reported_quota_leaves_out_what_is_held() -> eyre::Result<()> {
    const DEV_KEY: &str = "e2e-dev-key";

    let mut env = Environment::setup().await?;
    let cap = U256::from(u128::MAX);
    let sponsorship = SponsorshipConfig {
        quota_overrides: [("dev-local".to_string(), u128::MAX)].into(),
        ..limited(&env, cap, U256::MAX)
    };
    env.config.auth = Some(AuthConfig {
        jwks_url: "http://127.0.0.1:1/jwks".to_string(),
        dev_api_key: Some(DEV_KEY.to_string()),
        dev_subject: None,
    });
    let key = account(&mut env, sponsorship).await?;
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(&env, &key).await?;
    let with_token = HttpClientBuilder::default()
        .set_headers(HeaderMap::from_iter([(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {DEV_KEY}"))?,
        )]))
        .build(env.relay_handle.http_url())?;
    let quota = with_token
        .sponsorship_quota(SponsorshipQuotaParameters {
            chain_id: env.chain_id(),
            account: Some(env.eoa.address()),
        })
        .await?;

    let held = held(&env).await?;
    assert!(!held.is_zero() && held < max_cost(&response));
    assert_eq!(quota.remaining_wei, cap - held);
    Ok(())
}
