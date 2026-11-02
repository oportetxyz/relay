use crate::e2e::{
    environment::mint_erc20s,
    eoa::{MockAccount, MockAccountBuilder},
    *,
};
use alloy::{
    primitives::{Address, Bytes, U256},
    providers::ext::AnvilApi,
};
use chrono::Utc;
use eyre::Result;
use relay::{
    asset::AssetInfoService,
    rpc::{RelayApiClient, adjust_balance_for_decimals},
    storage::StorageApi,
    transactions::asset_diffs::trace_logs,
    types::{
        Asset, AssetDiffs, Call, DiffDirection, HistoricalPrice, IERC20,
        rpc::{BundleId, CallHistoryEntry, GetCallsHistoryParameters, SortDirection},
        simulation_assets::AssetDiffsBuilder,
    },
};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn test_multichain_calls_history_mixed_bundles() -> Result<()> {
    let env = Environment::setup_multi_chain(2).await?;

    // Create fresh account without ERC20 minting (includes initialization bundle)
    let account = MockAccountBuilder::new().no_erc20_mint().build(&env).await?;

    // Manually mint ERC20 on chain0 and chain1
    mint_erc20s(&[env.erc20], &[account.address], env.provider_for(0)).await?;
    mint_erc20s(&[env.erc20], &[account.address], env.provider_for(1)).await?;

    // Bundle 1: Single-chain on chain0
    let bundle1 = send_bundle(&env, &account, 0, U256::from(1), env.erc20).await?;

    // Sleep to ensure different timestamps for price testing
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Bundle 2: Single-chain on chain1
    let bundle2 = send_bundle(&env, &account, 1, U256::from(1), env.erc20).await?;

    // Sleep to ensure different timestamps for price testing
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Bundle 3: Interop - move more funds than available on chain1
    let bundle3 = {
        // Get decimals for both chains
        let decimals_0 = IERC20::new(env.erc20, env.provider_for(0)).decimals().call().await?;
        let decimals_1 = IERC20::new(env.erc20, env.provider_for(1)).decimals().call().await?;

        // Get balances on both chains
        let balance_chain0 =
            IERC20::new(env.erc20, env.provider_for(0)).balanceOf(account.address).call().await?;
        let balance_chain1 =
            IERC20::new(env.erc20, env.provider_for(1)).balanceOf(account.address).call().await?;

        // Adjust balance from chain0 to chain1 decimals
        let balance_chain0_adjusted =
            adjust_balance_for_decimals(balance_chain0, decimals_0, decimals_1);

        // Try to send on chain1: chain1_balance + half of chain0_balance (requires cross-chain)
        let amount_to_send = balance_chain1 + (balance_chain0_adjusted / U256::from(2));

        send_bundle(&env, &account, 1, amount_to_send, env.erc20).await?
    };

    populate_historical_prices(&env, &[bundle1, bundle2, bundle3]).await?;

    // Test 1: Fetch all history (ascending order)
    let history = get_history(&env, account.address, None, 10, SortDirection::Asc).await?;
    assert_asset_diffs_populated(&history);
    assert_eq!(history.len(), 4, "Expected 4 bundles in history");

    let bundle_ids: Vec<_> = history.iter().map(|e| e.id).collect();
    assert!(bundle_ids.contains(&bundle1));
    assert!(bundle_ids.contains(&bundle2));
    assert!(bundle_ids.contains(&bundle3));

    // Verify the first entry (initialization) is not one of our sent bundles
    assert_ne!(history[0].id, bundle1, "Init bundle should not be bundle1");
    assert_ne!(history[0].id, bundle2, "Init bundle should not be bundle2");
    assert_ne!(history[0].id, bundle3, "Init bundle should not be bundle3");

    // Verify bundle3 (interop) has quotes from both src and dst transactions
    let bundle3_entry = history.iter().find(|e| e.id == bundle3).expect("bundle3 should exist");
    assert_eq!(
        bundle3_entry.capabilities.quotes.len(),
        2,
        "Interop bundle should have 2 quotes (1 from src_txs, 1 from dst_txs)"
    );

    // Test 2: Fetch history in descending order
    let history_desc = get_history(&env, account.address, None, 10, SortDirection::Desc).await?;
    assert_asset_diffs_populated(&history);

    assert_eq!(history_desc.len(), 4);
    // Verify it's in reverse order
    assert_eq!(history_desc[0].id, bundle3, "Most recent should be bundle3");
    assert_eq!(history_desc[3].id, history[0].id, "Oldest should match first in asc");

    // Test 3: Limit to 2 bundles
    let history_limited = get_history(&env, account.address, None, 2, SortDirection::Asc).await?;
    assert_asset_diffs_populated(&history);

    assert_eq!(history_limited.len(), 2);
    assert_eq!(history_limited[0].id, history[0].id);
    assert_eq!(history_limited[1].id, history[1].id);

    // Test 4: Use index to paginate (skip first 2, get next 2)
    let history_paginated =
        get_history(&env, account.address, Some(2), 2, SortDirection::Asc).await?;
    assert_asset_diffs_populated(&history);

    assert_eq!(history_paginated.len(), 2);
    assert_eq!(history_paginated[0].id, history[2].id);
    assert_eq!(history_paginated[1].id, history[3].id);

    // Test 5: Index beyond available items
    let history_beyond =
        get_history(&env, account.address, Some(10), 10, SortDirection::Asc).await?;
    assert_eq!(history_beyond.len(), 0);

    // Test 6: Verify timestamps are increasing (ascending order)
    for i in 0..history.len() - 1 {
        assert!(
            history[i].timestamp <= history[i + 1].timestamp,
            "Timestamps should be in ascending order"
        );
    }

    Ok(())
}

async fn send_bundle(
    env: &Environment,
    account: &MockAccount,
    chain_idx: usize,
    amount: U256,
    fee_token: Address,
) -> Result<BundleId> {
    let response = env
        .relay_endpoint
        .prepare_calls(PrepareCallsParameters {
            calls: vec![Call::transfer(env.erc20, Address::random(), amount)],
            chain_id: env.chain_id_for(chain_idx),
            from: Some(account.address),
            capabilities: PrepareCallsCapabilities {
                meta: Meta { fee_token: Some(fee_token), ..Default::default() },
                ..Default::default()
            },
            key: Some(account.key.to_call_key()),
            ..Default::default()
        })
        .await?;

    let is_multichain = response.context.quote().unwrap().ty().multi_chain_root.is_some();
    let signature = account.key.sign_payload_hash(response.digest).await?;
    let bundle_id = send_prepared_calls(env, &account.key, signature, response.context).await?;

    let status = await_calls_status(env, bundle_id).await?;
    assert!(status.status.is_confirmed());
    if is_multichain {
        assert!(status.capabilities.unwrap().interop_status.unwrap().is_done());
    }

    Ok(bundle_id)
}

/// Sends calls paying the fee in a token, and returns what was stored with what a trace shows.
async fn stored_and_traced(
    env: &Environment,
    account: &MockAccount,
    call: Call,
) -> Result<(AssetDiffs, AssetDiffs)> {
    let response = env
        .relay_endpoint
        .prepare_calls(PrepareCallsParameters {
            calls: vec![call],
            chain_id: env.chain_id(),
            from: Some(account.address),
            capabilities: PrepareCallsCapabilities {
                meta: Meta { fee_token: Some(env.erc20), ..Default::default() },
                ..Default::default()
            },
            key: Some(account.key.to_call_key()),
            ..Default::default()
        })
        .await?;
    let signature = account.key.sign_payload_hash(response.digest).await?;
    let bundle_id = send_prepared_calls(env, &account.key, signature, response.context).await?;
    let status = await_calls_status(env, bundle_id).await?;
    assert!(status.status.is_confirmed());

    let storage = &env.relay_handle.storage;
    let tx_ids = storage.get_bundle_transactions(bundle_id).await?;
    // why: diffs are written by a task spawned after confirmation.
    let mut stored = None;
    for _ in 0..100 {
        stored = storage.read_asset_diffs(tx_ids.clone()).await?.into_iter().flatten().next();
        if stored.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let service = AssetInfoService::new(16, &env.config);
    let handle = service.handle();
    tokio::spawn(service);
    let logs = trace_logs(env.provider(), status.receipts[0].transaction_hash).await?;
    let builder = AssetDiffsBuilder::from_logs(&logs);
    let assets: Vec<Asset> = builder.seen_assets().copied().collect();
    let metadata = handle.get_asset_info_list(env.provider(), assets).await?;

    Ok((stored.expect("diffs were stored"), builder.build(metadata, Default::default())))
}

/// Every leg of the diffs, in an order that does not depend on how they were built.
fn legs(diffs: &AssetDiffs) -> Vec<(Address, Option<Address>, DiffDirection, U256)> {
    let mut legs: Vec<_> = diffs
        .0
        .iter()
        .flat_map(|(account, assets)| {
            assets.iter().map(|d| (*account, d.address, d.direction, d.value))
        })
        .collect();
    legs.sort_by_key(|(account, asset, _, value)| (*account, *asset, *value));
    legs
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_sends_store_what_a_trace_shows() -> Result<()> {
    let env = Environment::setup().await?;
    let account = MockAccountBuilder::new().build(&env).await?;
    env.provider().anvil_set_balance(account.address, U256::from(10u64.pow(18))).await?;
    let recipient = Address::random();

    let token_send = Call::transfer(env.erc20, recipient, U256::from(5));
    let (stored, traced) = stored_and_traced(&env, &account, token_send).await?;
    assert_eq!(legs(&stored), legs(&traced));
    assert!(legs(&stored).iter().any(|leg| leg.0 == recipient));

    let value = U256::from(1234);
    let native_send = Call { to: recipient, value, data: Bytes::new() };
    let (stored, traced) = stored_and_traced(&env, &account, native_send).await?;
    assert_eq!(legs(&stored), legs(&traced));
    assert!(legs(&stored).contains(&(account.address, None, DiffDirection::Outgoing, value)));
    assert!(legs(&stored).contains(&(recipient, None, DiffDirection::Incoming, value)));

    Ok(())
}

/// Helper to fetch call history with given parameters
async fn get_history(
    env: &Environment,
    address: Address,
    index: Option<u64>,
    limit: u64,
    sort: SortDirection,
) -> Result<Vec<CallHistoryEntry>> {
    env.relay_endpoint
        .get_calls_history(GetCallsHistoryParameters { address, index, limit, sort })
        .await
        .map_err(Into::into)
}

/// Helper to verify that asset diffs and fee totals are populated
fn assert_asset_diffs_populated(entries: &[CallHistoryEntry]) {
    for entry in entries {
        let asset_diff = &entry.capabilities.asset_diff;

        assert!(!asset_diff.fee_totals.is_empty());
        assert!(!asset_diff.asset_diffs.is_empty());

        for chain_diffs in asset_diff.asset_diffs.values() {
            for (_, diffs) in &chain_diffs.0 {
                for diff in diffs {
                    assert!(diff.fiat.is_some(),);
                }
            }
        }
    }
}

/// Test that verifies exact asset diff values match on-chain execution with the correct payer fee
/// removed (only the transfer amount should show up).
#[tokio::test(flavor = "multi_thread")]
async fn test_calls_history_exact_asset_diffs() -> Result<()> {
    let env = Environment::setup().await?;
    let account = MockAccountBuilder::new().build(&env).await?;
    let transfer_amount = U256::from(1000);
    let bundle_id = send_bundle(&env, &account, 0, transfer_amount, env.erc20).await?;
    populate_historical_prices(&env, &[bundle_id]).await?;

    let history = get_history(&env, account.address, None, 10, SortDirection::Asc).await?;
    let entry = history.iter().find(|e| e.id == bundle_id).unwrap();

    let quote = &entry.capabilities.quotes[0];
    let eoa_diffs = entry
        .capabilities
        .asset_diff
        .asset_diffs
        .get(&quote.chain_id)
        .unwrap()
        .0
        .iter()
        .find(|(addr, _)| addr == &account.address)
        .map(|(_, diffs)| diffs)
        .unwrap();

    let erc20_diff =
        eoa_diffs.iter().find(|d| d.address == Some(env.erc20)).expect("ERC20 diff should exist");

    assert_eq!(erc20_diff.value, transfer_amount,); // fee got removed correctly
    assert_eq!(erc20_diff.direction, DiffDirection::Outgoing,);
    assert!(!erc20_diff.recipients.is_empty(),);
    assert!(erc20_diff.fiat.is_some());

    let fee_total_price = entry
        .capabilities
        .asset_diff
        .fee_totals
        .get(&quote.chain_id)
        .expect("Fee total should exist for chain");

    assert!(fee_total_price.value > 0.0,);
    assert_eq!(fee_total_price.currency, "usd",);

    Ok(())
}

/// Helper to populate historical USD prices for bundles
async fn populate_historical_prices(env: &Environment, _bundle_ids: &[BundleId]) -> Result<()> {
    let now = Utc::now().timestamp() as u64;
    let (erc20_asset_uid, _) =
        env.relay_handle.chains.asset(env.chain_id(), env.erc20).expect("erc20 asset should exist");

    let mut prices = Vec::new();

    let (native_asset_uid, _) = env
        .relay_handle
        .chains
        .asset(env.chain_id(), Address::ZERO)
        .expect("native asset should exist");

    for asset_uid in [erc20_asset_uid, native_asset_uid] {
        prices.push(HistoricalPrice {
            asset_uid: asset_uid.clone(),
            timestamp: now - 60,
            usd_price: 1.0,
        });
        prices.push(HistoricalPrice {
            asset_uid: asset_uid.clone(),
            timestamp: now,
            usd_price: 2.0,
        });
        prices.push(HistoricalPrice {
            asset_uid: asset_uid.clone(),
            timestamp: now + 60,
            usd_price: 3.0,
        });
    }

    env.relay_handle.storage.store_historical_usd_prices(prices).await?;

    Ok(())
}

/// An account with `sends` bundles of its own on top of its initialization bundle, returned oldest
/// first.
async fn account_with_bundles(
    env: &Environment,
    sends: usize,
) -> Result<(MockAccount, Vec<BundleId>)> {
    let account = MockAccountBuilder::new().build(env).await?;
    let mut bundles = get_history(env, account.address, None, 100, SortDirection::Asc)
        .await?
        .iter()
        .map(|entry| entry.id)
        .collect::<Vec<_>>();
    assert_eq!(bundles.len(), 1, "the initialization bundle");

    for _ in 0..sends {
        // Bundles are ordered by time, so each one needs a timestamp of its own.
        tokio::time::sleep(Duration::from_secs(1)).await;
        bundles.push(send_bundle(env, &account, 0, U256::from(1), env.erc20).await?);
    }

    Ok((account, bundles))
}

fn ids_and_indexes(entries: &[CallHistoryEntry]) -> Vec<(BundleId, u64)> {
    entries.iter().map(|entry| (entry.id, entry.index)).collect()
}

/// The app reads its activity this way, and a page that started at the oldest bundle hid every
/// bundle newer than it.
#[tokio::test(flavor = "multi_thread")]
async fn a_newest_first_read_with_no_index_starts_at_the_newest() -> Result<()> {
    let env = Environment::setup().await?;
    let (account, bundles) = account_with_bundles(&env, 4).await?;
    let [init, first, second, third, fourth] = bundles[..] else { unreachable!() };

    let page = get_history(&env, account.address, None, 2, SortDirection::Desc).await?;
    assert_eq!(ids_and_indexes(&page), [(fourth, 0), (third, 1)]);

    let whole = get_history(&env, account.address, None, 5, SortDirection::Desc).await?;
    assert_eq!(
        ids_and_indexes(&whole),
        [(fourth, 0), (third, 1), (second, 2), (first, 3), (init, 4)],
        "a limit equal to the account's bundles returns all of them"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newest_first_read_under_the_limit_returns_everything() -> Result<()> {
    let env = Environment::setup().await?;
    let (account, bundles) = account_with_bundles(&env, 2).await?;
    let [init, first, second] = bundles[..] else { unreachable!() };

    let history = get_history(&env, account.address, None, 20, SortDirection::Desc).await?;
    assert_eq!(ids_and_indexes(&history), [(second, 0), (first, 1), (init, 2)]);

    Ok(())
}

/// Readers page through history with these, the server's vault basis among them.
#[tokio::test(flavor = "multi_thread")]
async fn reads_with_an_index_or_oldest_first_keep_their_pages() -> Result<()> {
    let env = Environment::setup().await?;
    let (account, bundles) = account_with_bundles(&env, 3).await?;
    let [init, first, second, third] = bundles[..] else { unreachable!() };
    let address = account.address;

    let desc = get_history(&env, address, Some(0), 2, SortDirection::Desc).await?;
    assert_eq!(ids_and_indexes(&desc), [(third, 0), (second, 1)]);
    let desc = get_history(&env, address, Some(2), 2, SortDirection::Desc).await?;
    assert_eq!(ids_and_indexes(&desc), [(first, 2), (init, 3)]);
    assert!(get_history(&env, address, Some(4), 2, SortDirection::Desc).await?.is_empty());

    let asc = get_history(&env, address, None, 2, SortDirection::Asc).await?;
    assert_eq!(ids_and_indexes(&asc), [(init, 0), (first, 1)]);
    let asc = get_history(&env, address, Some(2), 2, SortDirection::Asc).await?;
    assert_eq!(ids_and_indexes(&asc), [(second, 2), (third, 3)]);
    assert!(get_history(&env, address, Some(4), 2, SortDirection::Asc).await?.is_empty());

    Ok(())
}
