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
use relay::{
    rpc::RelayApiClient,
    signers::Eip712PayLoadSigner,
    storage::StorageApi,
    types::{
        Call, KeyType, KeyWith712Signer,
        OrchestratorContract::IntentExecuted,
        SponsorshipConfig, SponsorshipUsage,
        rpc::{
            BundleId, Meta, PrepareCallsCapabilities, PrepareCallsParameters, PrepareCallsResponse,
        },
    },
};
use sqlx::{PgPool, raw_sql};
use std::time::Duration;

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

/// The ledger's total for the test account.
pub(super) async fn booked(env: &Environment) -> eyre::Result<U256> {
    Ok(env
        .relay_handle
        .storage
        .sponsored_wei_in_window(&env.eoa.address().to_string(), env.chain_id(), 24)
        .await?)
}

/// Fails unless the ledger holds exactly `expected`. A final status means the booking is done.
pub(super) async fn assert_booked(env: &Environment, expected: U256) -> eyre::Result<()> {
    assert_eq!(booked(env).await?, expected);
    Ok(())
}

/// Prepares the test account's intent.
async fn prepare(env: &Environment, key: &KeyWith712Signer) -> eyre::Result<PrepareCallsResponse> {
    Ok(env
        .relay_endpoint
        .prepare_calls(PrepareCallsParameters {
            from: Some(env.eoa.address()),
            calls: vec![Call::transfer(env.erc20, Address::random(), U256::from(1))],
            chain_id: env.chain_id(),
            key: Some(key.to_call_key()),
            capabilities: PrepareCallsCapabilities {
                meta: Meta { fee_payer: None, fee_token: Some(env.fee_token), nonce: None },
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

/// Sends an intent, makes `outcome` true on chain, and returns what the relay paid.
async fn land(env: &Environment, key: &KeyWith712Signer, outcome: Outcome) -> eyre::Result<U256> {
    mint_erc20s(&[env.erc20], &[env.eoa.address()], env.provider()).await?;

    let response = prepare(env, key).await?;
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
    let sponsorship = SponsorshipConfig {
        sponsored_chains: [env.chain_id()].into(),
        whitelisted_contracts: std::iter::once(env.erc20).collect(),
        per_user_wei: U256::from(1),
        circuit_breaker_wei: U256::MAX,
        window_hours: 24,
        ..Default::default()
    };
    let key = account(&mut env, sponsorship).await?;
    let erc20_code = env.provider().get_code_at(env.erc20).await?;

    assert!(charged(&prepare(&env, &key).await?).is_zero());
    land(&env, &key, Outcome::CallReverts).await?;

    env.provider().anvil_set_code(env.erc20, erc20_code).await?;
    assert!(!charged(&prepare(&env, &key).await?).is_zero());

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
