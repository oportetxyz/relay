//! `wallet_executePreCalls`: landing stored precalls with a relay-paid transaction.
//!
//! A precall stored through `wallet_sendPreparedCalls` only reaches the chain
//! inside the next intent signed by the key it authorizes. These cases cover
//! the key nobody in the app can sign with: a recovery key granted as admin,
//! held in a wallet the relay never sees.

use crate::e2e::{
    AuthKind,
    cases::{
        sponsorship::{
            REVERTS, TOKEN_DEV_KEY, assert_booked, assert_not_in_token, booked, client_with,
            latest_transaction, serve_jwks, token, token_auth, wait_for_mempool,
        },
        upgrade_account_eagerly, upgrade_account_lazily,
    },
    environment::Environment,
    send_prepared_calls,
};
use alloy::{
    primitives::{Address, B256, U256},
    providers::ext::AnvilApi,
};
use jsonrpsee::http_client::HttpClientBuilder;
use relay::{
    rpc::RelayApiClient,
    signers::Eip712PayLoadSigner,
    storage::StorageApi,
    types::{
        KeyType, KeyWith712Signer, MULTICHAIN_NONCE_PREFIX, SignedCalls, SponsorshipConfig,
        rpc::{
            ExecutePreCallsParameters, Meta, PrepareCallsCapabilities, PrepareCallsParameters,
            PrepareCallsResponse,
        },
    },
};

/// The nonce shape the relay mints for its own account-creation precall: the
/// multichain prefix, a random sequence key, sequence 0. It drops the chain id
/// from the digest, so one signature stores on every chain.
fn multichain_precall_nonce() -> U256 {
    (MULTICHAIN_NONCE_PREFIX << 240) | ((U256::from_be_bytes(B256::random().into()) >> 80) << 64)
}

/// The relay pays for `wallet_executePreCalls`, so the sponsorship policy is
/// its gate; the e2e default sponsors nothing.
async fn sponsor_everything(env: &mut Environment) -> eyre::Result<()> {
    let mut config = env.config.clone();
    config.sponsorship = SponsorshipConfig { sponsor_all: true, ..Default::default() };
    env.restart_relay(config).await
}

/// Prepares a precall granting `recovery` as admin on `chain_id`, under `nonce`.
async fn prepare_admin_grant(
    env: &Environment,
    admin: &KeyWith712Signer,
    recovery: &KeyWith712Signer,
    chain_id: u64,
    nonce: U256,
) -> eyre::Result<PrepareCallsResponse> {
    let response = env
        .relay_endpoint
        .prepare_calls(PrepareCallsParameters {
            from: Some(env.eoa.address()),
            calls: vec![],
            chain_id,
            capabilities: PrepareCallsCapabilities {
                authorize_keys: vec![recovery.to_authorized()],
                revoke_keys: vec![],
                meta: Meta { fee_payer: None, fee_token: Some(env.fee_token), nonce: Some(nonce) },
                pre_calls: vec![],
                pre_call: true,
                required_funds: vec![],
            },
            state_overrides: Default::default(),
            balance_overrides: Default::default(),
            key: Some(admin.to_call_key()),
        })
        .await?;

    let precall = response.context.clone().take_precall().expect("a precall context");
    assert!(precall.call.is_multichain(), "a client nonce with the prefix must be kept verbatim");

    Ok(response)
}

/// Stores a precall that grants `recovery` as admin, signed by `admin`.
async fn store_admin_grant(
    env: &Environment,
    admin: &KeyWith712Signer,
    recovery: &KeyWith712Signer,
) -> eyre::Result<()> {
    let response =
        prepare_admin_grant(env, admin, recovery, env.chain_id(), multichain_precall_nonce())
            .await?;
    let signature = admin.sign_payload_hash(response.digest).await?;
    send_prepared_calls(env, admin, signature, response.context).await?;

    Ok(())
}

async fn execute(env: &Environment) -> eyre::Result<Vec<U256>> {
    Ok(env
        .relay_endpoint
        .execute_pre_calls(ExecutePreCallsParameters {
            address: env.eoa.address(),
            chain_id: env.chain_id(),
        })
        .await?
        .nonces)
}

async fn assert_recovery_key_on_chain(
    env: &Environment,
    recovery: &KeyWith712Signer,
    expected: bool,
) {
    let keys = env.get_eoa_authorized_keys().await.unwrap();
    let found = keys.iter().find(|key| key.hash == recovery.key_hash());

    assert_eq!(found.is_some(), expected, "recovery key on chain");

    if let Some(found) = found {
        assert!(found.authorize_key.key.isSuperAdmin, "granted as admin");
    }
}

/// The relay paid for a transaction that reverts as much as for one that lands.
#[tokio::test(flavor = "multi_thread")]
async fn books_a_landing_that_reverts() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    sponsor_everything(&mut env).await?;

    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_eagerly(&env, &[admin.to_authorized()], &admin, AuthKind::Auth).await?;
    store_admin_grant(&env, &admin, &recovery).await?;

    // The upgrade above was sponsored too.
    let before = booked(&env).await?;

    env.disable_mining().await;
    let (landing, mined) = tokio::join!(execute(&env), async {
        wait_for_mempool(&env).await?;
        env.provider().anvil_set_code(env.orchestrator, REVERTS).await?;
        env.mine_block().await;
        latest_transaction(&env).await
    });
    let (receipt, spent) = mined?;

    assert!(!receipt.status());
    assert!(landing.is_err_and(|err| err.to_string().contains("reverted")));
    assert_booked(&env, before + spent).await
}

/// The relay pays once the transaction is out, so the caller leaving changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn books_a_landing_the_caller_left() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    sponsor_everything(&mut env).await?;

    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_eagerly(&env, &[admin.to_authorized()], &admin, AuthKind::Auth).await?;
    store_admin_grant(&env, &admin, &recovery).await?;
    let before = booked(&env).await?;

    env.disable_mining().await;
    let mut landing = Box::pin(execute(&env));
    tokio::select! {
        result = &mut landing => eyre::bail!("the landing finished unmined: {result:?}"),
        sent = wait_for_mempool(&env) => sent?,
    }
    // why: dropping the request closes its connection, which ends the handler.
    drop(landing);

    env.mine_block().await;
    let (receipt, spent) = latest_transaction(&env).await?;
    assert!(receipt.status());

    let expected = before + spent;
    for _ in 0..50 {
        if booked(&env).await? >= expected {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_booked(&env, expected).await?;

    // why: the handler removes the precalls once it sees the status, so finding them here shows
    // it was really gone. A live handler would have had this long to remove them.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let stored = env.relay_handle.storage.read_precalls_for_eoa(env.chain_id(), env.eoa.address());
    assert!(!stored.await?.is_empty(), "the handler outlived the caller");
    Ok(())
}

/// The account has never been used on this chain: the relay must carry the 7702
/// authorization and the init precall alongside the stored grant.
#[tokio::test(flavor = "multi_thread")]
async fn lands_a_stored_grant_on_an_undelegated_account() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    sponsor_everything(&mut env).await?;

    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_lazily(&env, &[admin.to_authorized()], AuthKind::Auth).await?;

    store_admin_grant(&env, &admin, &recovery).await?;
    assert_recovery_key_on_chain(&env, &recovery, false).await;

    assert_eq!(execute(&env).await?.len(), 1);
    assert_recovery_key_on_chain(&env, &recovery, true).await;

    // Landed precalls leave storage, so a second call has nothing to do.
    assert!(execute(&env).await.is_err_and(|err| err.to_string().contains("no stored precalls")));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn lands_a_stored_grant_on_a_delegated_account() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    sponsor_everything(&mut env).await?;

    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_eagerly(&env, &[admin.to_authorized()], &admin, AuthKind::Auth).await?;

    store_admin_grant(&env, &admin, &recovery).await?;
    assert_recovery_key_on_chain(&env, &recovery, false).await;

    assert_eq!(execute(&env).await?.len(), 1);
    assert_recovery_key_on_chain(&env, &recovery, true).await;

    Ok(())
}

/// Storing costs the relay nothing; landing does, so the policy decides.
#[tokio::test(flavor = "multi_thread")]
async fn refuses_to_land_when_not_sponsored() -> eyre::Result<()> {
    let env = Environment::setup().await?;

    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_eagerly(&env, &[admin.to_authorized()], &admin, AuthKind::Auth).await?;

    store_admin_grant(&env, &admin, &recovery).await?;

    assert!(execute(&env).await.is_err_and(|err| err.to_string().contains("not sponsored")));
    assert_recovery_key_on_chain(&env, &recovery, false).await;

    Ok(())
}

/// A stored grant is sponsored without a quote, so nothing else holds its cost.
#[tokio::test(flavor = "multi_thread")]
async fn a_landing_is_held_like_a_quote() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_eagerly(&env, &[admin.to_authorized()], &admin, AuthKind::Auth).await?;
    store_admin_grant(&env, &admin, &recovery).await?;

    let policy = SponsorshipConfig {
        sponsored_chains: [env.chain_id()].into(),
        whitelisted_contracts: [env.eoa.address(), alloy::primitives::Address::ZERO]
            .into_iter()
            .collect(),
        per_user_wei: U256::MAX,
        circuit_breaker_wei: U256::MAX,
        ..Default::default()
    };
    let base = env.config.clone();
    let under = |sponsorship: SponsorshipConfig| {
        let mut config = base.clone();
        config.sponsorship = sponsorship;
        config
    };

    let over_the_gas_cap = SponsorshipConfig { max_gas_per_tx: 21_000, ..policy.clone() };
    env.restart_relay(under(over_the_gas_cap)).await?;
    store_admin_grant(&env, &admin, &recovery).await?;
    assert!(execute(&env).await.is_err_and(|err| err.to_string().contains("not sponsor")));

    let over_the_quota = SponsorshipConfig { per_user_wei: U256::ONE, ..policy.clone() };
    env.restart_relay(under(over_the_quota)).await?;
    store_admin_grant(&env, &admin, &recovery).await?;
    assert!(execute(&env).await.is_err_and(|err| err.to_string().contains("not sponsor")));

    env.restart_relay(under(policy)).await?;
    store_admin_grant(&env, &admin, &recovery).await?;
    execute(&env).await?;
    assert_recovery_key_on_chain(&env, &recovery, true).await;

    let subject = env.eoa.address().to_string();
    let held = env.relay_handle.storage.held_sponsorship_wei(&subject, env.chain_id());
    assert_eq!(held.await?, U256::ZERO);
    Ok(())
}

/// What the multichain nonce buys: one signature, stored on every chain, landed
/// on every chain, with no per-chain ceremony.
#[tokio::test(flavor = "multi_thread")]
async fn one_signature_lands_on_every_chain() -> eyre::Result<()> {
    let mut env = Environment::setup_multi_chain(2).await?;
    sponsor_everything(&mut env).await?;

    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_lazily(&env, &[admin.to_authorized()], AuthKind::Auth).await?;

    let nonce = multichain_precall_nonce();
    let mut digests = Vec::new();
    let mut contexts = Vec::new();
    for index in 0..env.num_chains() {
        let response =
            prepare_admin_grant(&env, &admin, &recovery, env.chain_id_for(index), nonce).await?;
        digests.push(response.digest);
        contexts.push(response.context);
    }
    assert!(digests.iter().all(|digest| digest == &digests[0]), "no chain id in the digest");

    let signature = admin.sign_payload_hash(digests[0]).await?;
    for context in contexts {
        send_prepared_calls(&env, &admin, signature.clone(), context).await?;
    }

    for index in 0..env.num_chains() {
        env.relay_endpoint
            .execute_pre_calls(ExecutePreCallsParameters {
                address: env.eoa.address(),
                chain_id: env.chain_id_for(index),
            })
            .await?;

        let keys = env.get_eoa_authorized_keys_on_chain(index).await?;
        assert!(
            keys.iter().any(|key| key.hash == recovery.key_hash()),
            "recovery key on chain {index}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_landing_needs_the_address_in_the_token() -> eyre::Result<()> {
    let mut env = Environment::setup().await?;
    let (signing, jwks_url) = serve_jwks().await?;
    env.config.auth = Some(token_auth(jwks_url, true));
    let mut config = env.config.clone();
    config.sponsorship = SponsorshipConfig {
        sponsor_all: true,
        quota_overrides: [("dev-local".to_string(), u128::MAX)].into(),
        ..Default::default()
    };
    env.restart_relay(config).await?;
    // why: the setup sends no token of its own, which this relay refuses.
    let without_token = HttpClientBuilder::default().build(env.relay_handle.http_url())?;
    env.relay_endpoint = client_with(&env, TOKEN_DEV_KEY)?;

    let admin = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    let recovery = KeyWith712Signer::random_admin(KeyType::Secp256k1)?.unwrap();
    upgrade_account_eagerly(&env, &[admin.to_authorized()], &admin, AuthKind::Auth).await?;
    store_admin_grant(&env, &admin, &recovery).await?;

    let parameters =
        || ExecutePreCallsParameters { address: env.eoa.address(), chain_id: env.chain_id() };
    assert_not_in_token(without_token.execute_pre_calls(parameters()).await.map_err(Into::into));

    let outside = serde_json::json!([format!("{:#x}", Address::random())]);
    let client = client_with(&env, &token(&signing, Some(outside)))?;
    assert_not_in_token(client.execute_pre_calls(parameters()).await.map_err(Into::into));

    let listed = serde_json::json!([format!("{:#x}", env.eoa.address())]);
    let client = client_with(&env, &token(&signing, Some(listed)))?;
    assert!(!client.execute_pre_calls(parameters()).await?.nonces.is_empty());
    Ok(())
}
