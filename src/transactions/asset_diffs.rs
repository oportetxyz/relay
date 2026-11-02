//! Asset diffs of a landed transaction, built after it confirmed.
//!
//! An intent that only moves tokens or native value is fully described by its own calls and its
//! receipt. Anything else needs the trace, which a node may not serve until the block is sealed
//! even though it already returns the receipt, so reads here are retried.
//!
//! This runs in a task spawned after confirmation: it cannot delay or fail a send.

use crate::{
    asset::{self, AssetInfoServiceHandle},
    constants::SIMULATEV1_NATIVE_ADDRESS,
    error::RelayError,
    transactions::{RelayTransaction, RelayTransactionKind},
    types::{
        Asset, AssetDiffs, Call, IERC20, Intent, OrchestratorContract::IntentExecuted, SignedCall,
        rpc::BundleId, simulation_assets::AssetDiffsBuilder,
    },
};
use alloy::{
    primitives::{Address, B256, ChainId, Log, U256, b256},
    providers::{Provider, ext::DebugApi},
    rpc::types::{
        TransactionReceipt,
        trace::geth::{CallConfig, GethDebugTracingOptions},
    },
    sol_types::{SolCall, SolEvent, SolValue},
    transports::TransportErrorKind,
};
use std::{fmt::Display, future::Future, time::Duration};
use tokio::time::Instant;
use tracing::{error, info, warn};

/// Waits between attempts of a read: six attempts over about 15 seconds.
///
/// A trace for a transaction read from a pending block was served after about 2 seconds.
pub const DIFF_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// `Approval(address,address,uint256)`, which the `IERC20` binding leaves out.
const APPROVAL_EVENT: B256 =
    b256!("0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925");

/// How a landed transaction's diffs have to be built.
#[derive(Debug, PartialEq, Eq)]
pub enum LandedKind {
    /// Its calls and receipt describe it. Holds the native value the account sent by its calls.
    Plain(Vec<NativeSend>),
    /// Needs the trace, for the given reason.
    Complex(&'static str),
    /// Its calls could not be read, so it is treated as complex.
    Unreadable(String),
}

/// Native value sent by one of an intent's calls.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct NativeSend {
    /// Account the value left.
    pub from: Address,
    /// Account the value went to.
    pub to: Address,
    /// Amount sent.
    pub value: U256,
}

/// What one call of an intent does, as far as classification is concerned.
enum CallKind {
    Native(NativeSend),
    /// A token call, plain once the receipt shows the contract emitting the given event.
    Token {
        contract: Address,
        event: B256,
    },
    Other,
}

/// Classifies a call. A bare selector match is not enough: the payload must be exactly the one
/// the function takes, and it must carry no value.
fn call_kind(eoa: Address, call: &Call) -> CallKind {
    // why: the account reads a zero target as itself, and a call to itself is account management.
    if call.to.is_zero() || call.to == eoa {
        return CallKind::Other;
    }
    if call.data.is_empty() {
        return if call.value.is_zero() {
            CallKind::Other
        } else {
            CallKind::Native(NativeSend { from: eoa, to: call.to, value: call.value })
        };
    }
    if !call.value.is_zero() {
        return CallKind::Other;
    }

    let event = match call.data.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) {
        Some(IERC20::transferCall::SELECTOR) if call.data.len() == 4 + 2 * 32 => {
            IERC20::Transfer::SIGNATURE_HASH
        }
        Some(IERC20::transferFromCall::SELECTOR) if call.data.len() == 4 + 3 * 32 => {
            IERC20::Transfer::SIGNATURE_HASH
        }
        Some(IERC20::approveCall::SELECTOR) if call.data.len() == 4 + 2 * 32 => APPROVAL_EVENT,
        _ => return CallKind::Other,
    };
    CallKind::Token { contract: call.to, event }
}

/// Classifies a landed transaction from the calls the relay encoded. When unsure it is complex.
pub fn classify(tx: &RelayTransaction, receipt: &TransactionReceipt) -> LandedKind {
    let RelayTransactionKind::Intent { quote, .. } = &tx.kind else {
        return LandedKind::Complex("internal transaction");
    };
    classify_intent(&quote.intent, receipt)
}

fn classify_intent(intent: &Intent, receipt: &TransactionReceipt) -> LandedKind {
    let eoa = *intent.eoa();

    if intent.is_interop() || !intent.encoded_fund_transfers().is_empty() {
        return LandedKind::Complex("cross-chain intent");
    }
    // why: the fee leaves as a native transfer that no call and no log shows, and the read path
    // takes the fee off the account's native leg, so a plain build would come out short.
    if intent.payment_token().is_zero() && !intent.total_payment_max_amount().is_zero() {
        return LandedKind::Complex("fee paid in native value");
    }
    // why: a call that did not run moved nothing, so its native value must not become a leg.
    let executed: Vec<_> = receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<IntentExecuted>().ok())
        .map(|log| log.inner.data)
        .collect();
    if !receipt.status() || executed.is_empty() || executed.iter().any(IntentExecuted::has_error) {
        return LandedKind::Complex("intent did not execute cleanly");
    }

    let mut batches = vec![intent.execution_data().clone()];
    for encoded in intent.encoded_pre_calls() {
        match SignedCall::abi_decode(encoded) {
            Ok(pre_call) if pre_call.eoa == eoa => batches.push(pre_call.executionData),
            Ok(_) => return LandedKind::Complex("pre-call of another account"),
            Err(err) => return LandedKind::Unreadable(err.to_string()),
        }
    }

    let mut native = Vec::new();
    for batch in batches {
        let calls = match <Vec<Call>>::abi_decode(&batch) {
            Ok(calls) => calls,
            Err(err) => return LandedKind::Unreadable(err.to_string()),
        };
        for call in &calls {
            match call_kind(eoa, call) {
                CallKind::Native(send) => native.push(send),
                CallKind::Token { contract, event } => {
                    // why: any contract can expose these selectors. One that emitted the
                    // standard event for the call is a token; one that did not is unknown.
                    if !receipt
                        .logs()
                        .iter()
                        .any(|l| l.address() == contract && l.topic0() == Some(&event))
                    {
                        return LandedKind::Complex("token call without its event");
                    }
                }
                CallKind::Other => return LandedKind::Complex("call to a contract"),
            }
        }
    }
    LandedKind::Plain(native)
}

/// Logs of a transaction's trace, native value included as transfer logs.
pub async fn trace_logs<P: Provider>(provider: &P, tx_hash: B256) -> Result<Vec<Log>, RelayError> {
    let options = GethDebugTracingOptions::call_tracer(CallConfig::default().with_log());
    let trace = provider.debug_trace_transaction(tx_hash, options).await?;
    let frame = trace.try_into_call_frame().map_err(|e| {
        TransportErrorKind::custom_str(&format!("Failed to extract call frame from trace: {e}"))
    })?;
    Ok(crate::types::simulator::collect_calls_and_logs_from_frame(frame).1)
}

fn receipt_logs(receipt: &TransactionReceipt) -> Vec<Log> {
    receipt.logs().iter().map(|log| log.inner.clone()).collect()
}

/// Which read a log line is about.
#[derive(Debug, Clone, Copy)]
enum Step {
    Classify,
    Trace,
    Receipt,
    Metadata,
}

/// What every log line carries.
#[derive(Debug, Clone, Copy)]
struct Landed {
    tx_hash: B256,
    chain_id: ChainId,
    bundle_id: Option<BundleId>,
}

/// Replaces URLs in an error, since a transport error can quote the endpoint and its key.
fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let start = rest[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'))
            .map_or(0, |i| i + 1);
        out.push_str(&rest[..start]);
        out.push_str("<url>");
        rest = &rest[at + 3..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\'' | '>' | ','))
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Runs a read until it works or the delays run out, logging every failure with its reason.
async fn with_retry<T, E, F, Fut>(
    step: Step,
    landed: Landed,
    delays: &[Duration],
    mut read: F,
) -> Option<T>
where
    E: Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let Landed { tx_hash, chain_id, bundle_id } = landed;
    let started = Instant::now();
    let mut delays = delays.iter();
    let mut attempt = 1u32;
    loop {
        let err = match read().await {
            Ok(value) => {
                if attempt > 1 {
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    info!(?step, %tx_hash, chain_id, ?bundle_id, attempt, elapsed_ms, "asset diff read recovered");
                }
                return Some(value);
            }
            Err(err) => scrub(&err.to_string()),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let Some(delay) = delays.next() else {
            error!(?step, %tx_hash, chain_id, ?bundle_id, attempt, elapsed_ms, error = %err, "asset diff read gave up");
            return None;
        };
        warn!(?step, %tx_hash, chain_id, ?bundle_id, attempt, elapsed_ms, error = %err, "asset diff read failed, retrying");
        tokio::time::sleep(*delay).await;
        attempt += 1;
    }
}

/// Builds the diffs of a landed transaction, or logs why it could not.
pub async fn landed_asset_diffs<P: Provider>(
    provider: &P,
    asset_info: &AssetInfoServiceHandle,
    tx: &RelayTransaction,
    receipt: &TransactionReceipt,
    chain_id: ChainId,
    delays: &[Duration],
) -> Option<AssetDiffs> {
    let tx_hash = receipt.transaction_hash;
    let bundle_id = tx.bundle_id;
    let landed = Landed { tx_hash, chain_id, bundle_id };
    let started = Instant::now();

    let kind = classify(tx, receipt);
    if let LandedKind::Unreadable(err) = &kind {
        let step = Step::Classify;
        warn!(?step, %tx_hash, chain_id, ?bundle_id, attempt = 1, elapsed_ms = 0, error = %scrub(err), "could not read the intent's calls, treating it as complex");
    }

    let builder = match kind {
        LandedKind::Plain(native) => {
            let mut builder = AssetDiffsBuilder::from_logs(&receipt_logs(receipt));
            for NativeSend { from, to, value } in native {
                builder.record_erc20(
                    SIMULATEV1_NATIVE_ADDRESS.into(),
                    IERC20::Transfer { from, to, amount: value },
                );
            }
            builder
        }
        LandedKind::Complex(_) | LandedKind::Unreadable(_) => {
            let logs =
                with_retry(Step::Trace, landed, delays, || trace_logs(provider, tx_hash)).await;
            logs.map_or_else(
                || {
                    let step = Step::Receipt;
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    warn!(?step, %tx_hash, chain_id, ?bundle_id, attempt = 1, elapsed_ms, error = "trace unavailable", "falling back to receipt logs, native value is not shown");
                    AssetDiffsBuilder::from_logs(&receipt_logs(receipt))
                },
                |logs| AssetDiffsBuilder::from_logs(&logs),
            )
        }
    };

    let assets: Vec<Asset> = builder.seen_assets().copied().collect();
    let nfts: Vec<_> = builder.seen_nfts().collect();
    let (metadata, uris) = with_retry(Step::Metadata, landed, delays, || async {
        tokio::try_join!(
            asset_info.get_asset_info_list(provider, assets.clone()),
            asset::fetch_nft_uris(provider, &nfts)
        )
    })
    .await?;

    Some(builder.build(metadata, uris))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asset::AssetInfoService,
        config::RelayConfig,
        types::{AssetDiff, AssetMetadata, AssetWithInfo, DiffDirection, IntentV05, Quote},
    };
    use alloy::{
        eips::eip1559::Eip1559Estimation,
        primitives::{Bytes, address, b256, bytes},
        providers::ProviderBuilder,
        rpc::{
            client::RpcClient,
            json_rpc::{ErrorPayload, RequestPacket, Response, ResponsePacket, ResponsePayload},
        },
        transports::{TransportError, TransportFut},
    };
    use serde_json::{Value, json, value::to_raw_value};
    use std::{
        collections::{HashMap, VecDeque},
        io,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };
    use tracing::instrument::WithSubscriber;

    const CHAIN: ChainId = 8453;
    const TX: B256 = b256!("0x890c5d18643e0000000000000000000000000000000000000000000000000000");
    const EOA: Address = address!("0x00000000000000000000000000000000000000aa");
    const FRIEND: Address = address!("0x00000000000000000000000000000000000000bb");
    const USDC: Address = address!("0x833589fcd6edb6e08f4c7c32d4f71b54bda02900");
    const ROUTER: Address = address!("0x00000000000000000000000000000000000000cc");
    const NO_DELAYS: [Duration; 5] = [Duration::ZERO; 5];
    const TRACE: &str = "debug_traceTransaction";
    const NOT_FOUND: &str = "transaction not found";
    /// An RPC error as a transport can word it, endpoint and key included.
    const KEYED_ERROR: &str = "request failed for https://node.example/secret-key (timeout)";

    type Replies = HashMap<String, VecDeque<Result<Value, String>>>;

    /// An RPC node that answers from a script and records what it was asked.
    #[derive(Clone, Default)]
    struct ScriptedRpc {
        asked: Arc<Mutex<Vec<String>>>,
        replies: Arc<Mutex<Replies>>,
    }

    impl ScriptedRpc {
        fn reply(&self, method: &str, reply: Result<Value, &str>) {
            self.replies
                .lock()
                .unwrap()
                .entry(method.to_string())
                .or_default()
                .push_back(reply.map_err(str::to_string));
        }

        fn fail(&self, method: &str, times: usize, error: &str) {
            (0..times).for_each(|_| self.reply(method, Err(error)));
        }

        fn asked(&self, method: &str) -> usize {
            self.asked.lock().unwrap().iter().filter(|m| *m == method).count()
        }
    }

    impl tower::Service<RequestPacket> for ScriptedRpc {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: RequestPacket) -> Self::Future {
            let RequestPacket::Single(request) = request else { panic!("no batches here") };
            let method = request.method().to_string();
            self.asked.lock().unwrap().push(method.clone());
            let scripted =
                self.replies.lock().unwrap().get_mut(&method).and_then(VecDeque::pop_front);
            let reply = scripted.unwrap_or_else(|| match method.as_str() {
                "eth_chainId" => Ok(json!(format!("{CHAIN:#x}"))),
                _ => Err(format!("nothing scripted for {method}")),
            });
            let payload = match reply {
                Ok(value) => ResponsePayload::Success(to_raw_value(&value).unwrap()),
                Err(message) => ResponsePayload::Failure(ErrorPayload {
                    code: -32000,
                    message: message.into(),
                    data: None,
                }),
            };
            let id = request.id().clone();
            Box::pin(async move { Ok(ResponsePacket::Single(Response { id, payload })) })
        }
    }

    fn word(address: Address) -> B256 {
        address.into_word()
    }

    fn amount(value: u64) -> String {
        format!("{:#x}", B256::from(U256::from(value)))
    }

    fn transfer_log(token: Address, from: Address, to: Address, value: u64) -> Value {
        json!({
            "address": token,
            "topics": [IERC20::Transfer::SIGNATURE_HASH, word(from), word(to)],
            "data": amount(value),
            "blockHash": B256::ZERO,
            "blockNumber": "0x1",
            "transactionHash": TX,
            "transactionIndex": "0x0",
            "logIndex": "0x0",
            "removed": false,
        })
    }

    fn intent_executed(err: [u8; 4]) -> Value {
        let mut data = [0u8; 64];
        data[32..36].copy_from_slice(&err);
        json!({
            "address": ROUTER,
            "topics": [IntentExecuted::SIGNATURE_HASH, word(EOA), B256::ZERO],
            "data": Bytes::copy_from_slice(&data),
            "blockHash": B256::ZERO,
            "blockNumber": "0x1",
            "transactionHash": TX,
            "transactionIndex": "0x0",
            "logIndex": "0x0",
            "removed": false,
        })
    }

    fn receipt_with(status: bool, logs: Vec<Value>) -> TransactionReceipt {
        serde_json::from_value(json!({
            "transactionHash": TX,
            "transactionIndex": "0x0",
            "blockHash": B256::ZERO,
            "blockNumber": "0x1",
            "from": EOA,
            "to": ROUTER,
            "contractAddress": null,
            "cumulativeGasUsed": "0x1",
            "gasUsed": "0x1",
            "effectiveGasPrice": "0x1",
            "logsBloom": format!("0x{}", "0".repeat(512)),
            "type": "0x2",
            "status": if status { "0x1" } else { "0x0" },
            "logs": logs,
        }))
        .unwrap()
    }

    /// A receipt of a clean intent with the given logs.
    fn receipt(mut logs: Vec<Value>) -> TransactionReceipt {
        logs.push(intent_executed([0; 4]));
        receipt_with(true, logs)
    }

    fn transfer_call(to: Address, value: u64) -> Call {
        Call {
            to: USDC,
            value: U256::ZERO,
            data: IERC20::transferCall { to, amount: U256::from(value) }.abi_encode().into(),
        }
    }

    fn native_call(to: Address, value: u64) -> Call {
        Call { to, value: U256::from(value), data: Bytes::new() }
    }

    fn swap_call() -> Call {
        Call { to: ROUTER, value: U256::ZERO, data: bytes!("0x5ae401dc00000000") }
    }

    fn intent(calls: Vec<Call>) -> IntentV05 {
        IntentV05 { eoa: EOA, executionData: calls.abi_encode().into(), ..Default::default() }
    }

    fn pre_call(eoa: Address, calls: Vec<Call>) -> Bytes {
        SignedCall {
            eoa,
            executionData: calls.abi_encode().into(),
            nonce: U256::ZERO,
            signature: Bytes::new(),
        }
        .abi_encode()
        .into()
    }

    fn landed(intent: IntentV05) -> RelayTransaction {
        let quote = Quote {
            chain_id: CHAIN,
            intent: Intent::V05(intent),
            extra_payment: U256::ZERO,
            eth_price: U256::ZERO,
            payment_token_decimals: 6,
            tx_gas: 0,
            native_fee_estimate: Eip1559Estimation {
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
            },
            authorization_address: None,
            additional_authorization: None,
            orchestrator: ROUTER,
            fee_token_deficit: U256::ZERO,
            asset_deficits: Default::default(),
            expected_cost: U256::ZERO,
        };
        RelayTransaction::new(quote, Vec::new(), B256::ZERO)
            .with_bundle_id(BundleId(B256::repeat_byte(7)))
    }

    fn kind_of(intent: IntentV05, receipt: &TransactionReceipt) -> LandedKind {
        classify(&landed(intent), receipt)
    }

    /// Runs the build against a scripted node, with USDC metadata cached as in production.
    async fn run(
        rpc: &ScriptedRpc,
        tx: &RelayTransaction,
        receipt: &TransactionReceipt,
    ) -> Option<AssetDiffs> {
        let service = AssetInfoService::new(8, &RelayConfig::default());
        let handle = service.handle();
        handle.seed(
            CHAIN,
            vec![AssetWithInfo {
                asset: Asset::Token(USDC),
                metadata: AssetMetadata {
                    name: Some("USD Coin".to_string()),
                    symbol: Some("USDC".to_string()),
                    decimals: Some(6),
                    uri: None,
                },
            }],
        );
        tokio::spawn(service);
        let provider = ProviderBuilder::new().connect_client(RpcClient::new(rpc.clone(), true));
        landed_asset_diffs(&provider, &handle, tx, receipt, CHAIN, &NO_DELAYS).await
    }

    fn leg(diffs: &AssetDiffs, account: Address, asset: Option<Address>) -> Option<AssetLeg> {
        let (_, legs) = diffs.0.iter().find(|(eoa, _)| *eoa == account)?;
        legs.iter().find(|d: &&AssetDiff| d.address == asset).map(|d| (d.direction, d.value))
    }

    type AssetLeg = (DiffDirection, U256);

    fn out(value: u64) -> Option<AssetLeg> {
        Some((DiffDirection::Outgoing, U256::from(value)))
    }

    fn inc(value: u64) -> Option<AssetLeg> {
        Some((DiffDirection::Incoming, U256::from(value)))
    }

    /// A call frame in which the router pays the account native value and USDC goes the other way.
    fn swap_trace() -> Value {
        json!({
            "from": EOA, "to": ROUTER, "type": "CALL", "gas": "0x1", "gasUsed": "0x1",
            "input": "0x", "value": "0x0",
            "logs": [{
                "address": USDC,
                "topics": [IERC20::Transfer::SIGNATURE_HASH, word(EOA), word(ROUTER)],
                "data": amount(5),
                "position": "0x0",
            }],
            "calls": [{
                "from": ROUTER, "to": EOA, "type": "CALL", "gas": "0x1", "gasUsed": "0x1",
                "input": "0x", "value": "0x9",
            }],
        })
    }

    #[tokio::test]
    async fn plain_usdc_send_needs_no_trace() {
        let rpc = ScriptedRpc::default();
        let tx = landed(intent(vec![transfer_call(FRIEND, 5)]));
        let receipt = receipt(vec![transfer_log(USDC, EOA, FRIEND, 5)]);

        let diffs = run(&rpc, &tx, &receipt).await.unwrap();

        assert_eq!(rpc.asked(TRACE), 0);
        assert_eq!(leg(&diffs, EOA, Some(USDC)), out(5));
        assert_eq!(leg(&diffs, FRIEND, Some(USDC)), inc(5));
    }

    #[tokio::test]
    async fn native_send_to_an_account_shows_its_leg() {
        let rpc = ScriptedRpc::default();
        let tx = landed(intent(vec![native_call(FRIEND, 9)]));

        let diffs = run(&rpc, &tx, &receipt(vec![])).await.unwrap();

        assert_eq!(rpc.asked(TRACE), 0);
        assert_eq!(leg(&diffs, EOA, None), out(9));
        assert_eq!(leg(&diffs, FRIEND, None), inc(9));
    }

    #[tokio::test]
    async fn swap_like_call_uses_the_trace() {
        let rpc = ScriptedRpc::default();
        rpc.reply(TRACE, Ok(swap_trace()));
        let tx = landed(intent(vec![swap_call()]));

        let diffs = run(&rpc, &tx, &receipt(vec![])).await.unwrap();

        assert_eq!(rpc.asked(TRACE), 1);
        assert_eq!(leg(&diffs, EOA, Some(USDC)), out(5));
        // Only the trace knows about this one: no call of the intent and no log shows it.
        assert_eq!(leg(&diffs, EOA, None), inc(9));
    }

    #[tokio::test]
    async fn trace_not_found_then_success() {
        let rpc = ScriptedRpc::default();
        rpc.fail(TRACE, 2, NOT_FOUND);
        rpc.reply(TRACE, Ok(swap_trace()));
        let tx = landed(intent(vec![swap_call()]));

        let diffs = run(&rpc, &tx, &receipt(vec![])).await.unwrap();

        assert_eq!(rpc.asked(TRACE), 3);
        assert_eq!(leg(&diffs, EOA, None), inc(9));
    }

    #[tokio::test]
    async fn trace_failing_throughout_falls_back_to_the_receipt() {
        let rpc = ScriptedRpc::default();
        rpc.fail(TRACE, 6, NOT_FOUND);
        let tx = landed(intent(vec![swap_call()]));
        let receipt = receipt(vec![transfer_log(USDC, EOA, ROUTER, 5)]);

        let diffs = run(&rpc, &tx, &receipt).await.unwrap();

        assert_eq!(rpc.asked(TRACE), 6);
        assert_eq!(leg(&diffs, EOA, Some(USDC)), out(5));
    }

    #[tokio::test]
    async fn metadata_is_retried() {
        let rpc = ScriptedRpc::default();
        rpc.fail("eth_chainId", 2, "header not found");
        let tx = landed(intent(vec![transfer_call(FRIEND, 5)]));
        let receipt = receipt(vec![transfer_log(USDC, EOA, FRIEND, 5)]);

        let diffs = run(&rpc, &tx, &receipt).await.unwrap();

        assert_eq!(rpc.asked("eth_chainId"), 3);
        assert_eq!(leg(&diffs, EOA, Some(USDC)), out(5));
    }

    #[tokio::test]
    async fn metadata_failing_throughout_stores_nothing() {
        let rpc = ScriptedRpc::default();
        rpc.fail("eth_chainId", 6, "header not found");
        let tx = landed(intent(vec![transfer_call(FRIEND, 5)]));
        let receipt = receipt(vec![transfer_log(USDC, EOA, FRIEND, 5)]);

        assert!(run(&rpc, &tx, &receipt).await.is_none());
        assert_eq!(rpc.asked("eth_chainId"), 6);
    }

    #[test]
    fn a_mixed_bundle_is_complex() {
        let calls = vec![transfer_call(FRIEND, 5), swap_call(), native_call(FRIEND, 1)];
        let receipt = receipt(vec![transfer_log(USDC, EOA, FRIEND, 5)]);
        assert_eq!(kind_of(intent(calls), &receipt), LandedKind::Complex("call to a contract"));
    }

    #[test]
    fn a_batch_of_plain_calls_is_plain() {
        let approve = Call {
            to: USDC,
            value: U256::ZERO,
            data: IERC20::approveCall { spender: ROUTER, amount: U256::ONE }.abi_encode().into(),
        };
        let pull = Call {
            to: USDC,
            value: U256::ZERO,
            data: IERC20::transferFromCall { from: FRIEND, to: EOA, amount: U256::ONE }
                .abi_encode()
                .into(),
        };
        let calls = vec![approve, pull, native_call(FRIEND, 3)];
        let mut approval = transfer_log(USDC, EOA, ROUTER, 1);
        approval["topics"][0] = json!(APPROVAL_EVENT);
        let receipt = receipt(vec![approval, transfer_log(USDC, FRIEND, EOA, 1)]);

        assert_eq!(
            kind_of(intent(calls), &receipt),
            LandedKind::Plain(vec![NativeSend { from: EOA, to: FRIEND, value: U256::from(3) }])
        );
    }

    #[test]
    fn a_token_selector_is_not_enough() {
        // A contract that takes `transfer` but emitted no Transfer is not known to be a token.
        let quiet = receipt(vec![]);
        assert_eq!(
            kind_of(intent(vec![transfer_call(FRIEND, 5)]), &quiet),
            LandedKind::Complex("token call without its event")
        );

        // The event must come from the contract that was called.
        let elsewhere = receipt(vec![transfer_log(ROUTER, EOA, FRIEND, 5)]);
        assert_eq!(
            kind_of(intent(vec![transfer_call(FRIEND, 5)]), &elsewhere),
            LandedKind::Complex("token call without its event")
        );

        let emitted = receipt(vec![transfer_log(USDC, EOA, FRIEND, 5)]);

        // A longer payload is some other function, or at least not one we can vouch for.
        let mut long = transfer_call(FRIEND, 5);
        long.data = [long.data.as_ref(), &[0u8; 32]].concat().into();
        assert_eq!(
            kind_of(intent(vec![long]), &emitted),
            LandedKind::Complex("call to a contract")
        );

        // Value on a token call means the callee does something with it.
        let mut paid = transfer_call(FRIEND, 5);
        paid.value = U256::ONE;
        assert_eq!(
            kind_of(intent(vec![paid]), &emitted),
            LandedKind::Complex("call to a contract")
        );
    }

    #[test]
    fn calls_to_the_account_itself_are_complex() {
        for to in [EOA, Address::ZERO] {
            assert_eq!(
                kind_of(intent(vec![native_call(to, 1)]), &receipt(vec![])),
                LandedKind::Complex("call to a contract")
            );
        }
        let empty = Call { to: FRIEND, value: U256::ZERO, data: Bytes::new() };
        assert_eq!(
            kind_of(intent(vec![empty]), &receipt(vec![])),
            LandedKind::Complex("call to a contract")
        );
    }

    #[test]
    fn pre_calls_are_held_to_the_same_rule() {
        let mut plain = intent(vec![native_call(FRIEND, 1)]);
        plain.encodedPreCalls = vec![pre_call(EOA, vec![native_call(FRIEND, 2)])];
        let LandedKind::Plain(sends) = kind_of(plain, &receipt(vec![])) else {
            panic!("a pre-call that only sends value is plain");
        };
        assert_eq!(sends.len(), 2, "both native sends are legs");

        let mut busy = intent(vec![native_call(FRIEND, 1)]);
        busy.encodedPreCalls = vec![pre_call(EOA, vec![swap_call()])];
        assert_eq!(kind_of(busy, &receipt(vec![])), LandedKind::Complex("call to a contract"));

        let mut foreign = intent(vec![native_call(FRIEND, 1)]);
        foreign.encodedPreCalls = vec![pre_call(FRIEND, vec![native_call(EOA, 1)])];
        assert_eq!(
            kind_of(foreign, &receipt(vec![])),
            LandedKind::Complex("pre-call of another account")
        );
    }

    #[test]
    fn calls_that_did_not_run_are_not_plain() {
        let calls = vec![native_call(FRIEND, 9)];
        let reason = LandedKind::Complex("intent did not execute cleanly");

        let failed = receipt_with(true, vec![intent_executed([0xde, 0xad, 0xbe, 0xef])]);
        assert_eq!(kind_of(intent(calls.clone()), &failed), reason);

        // A pre-call succeeding says nothing about the intent that followed it.
        let partly = receipt_with(true, vec![intent_executed([0; 4]), intent_executed([0xde; 4])]);
        assert_eq!(kind_of(intent(calls.clone()), &partly), reason);

        let reverted = receipt_with(false, vec![intent_executed([0; 4])]);
        assert_eq!(kind_of(intent(calls.clone()), &reverted), reason);
        assert_eq!(kind_of(intent(calls), &receipt_with(true, vec![])), reason);
    }

    #[test]
    fn a_fee_paid_in_native_value_is_complex() {
        let mut paying = intent(vec![native_call(FRIEND, 1)]);
        paying.paymentMaxAmount = U256::ONE;
        assert_eq!(
            kind_of(paying.clone(), &receipt(vec![])),
            LandedKind::Complex("fee paid in native value")
        );

        // The same fee in a token shows up in the logs.
        paying.paymentToken = USDC;
        assert!(matches!(kind_of(paying, &receipt(vec![])), LandedKind::Plain(_)));
    }

    #[test]
    fn funded_and_internal_transactions_are_complex() {
        let mut funded = intent(vec![native_call(FRIEND, 1)]);
        funded.encodedFundTransfers = vec![Bytes::new()];
        assert_eq!(kind_of(funded, &receipt(vec![])), LandedKind::Complex("cross-chain intent"));

        let internal = RelayTransaction::new_internal(ROUTER, Bytes::new(), CHAIN, 21_000);
        assert_eq!(
            classify(&internal, &receipt(vec![])),
            LandedKind::Complex("internal transaction")
        );
    }

    #[test]
    fn unreadable_calls_are_not_plain() {
        let garbage =
            IntentV05 { eoa: EOA, executionData: bytes!("0xdeadbeef"), ..Default::default() };
        assert!(matches!(kind_of(garbage, &receipt(vec![])), LandedKind::Unreadable(_)));
    }

    #[test]
    fn retries_span_about_fifteen_seconds() {
        assert_eq!(DIFF_RETRY_DELAYS.iter().sum::<Duration>(), Duration::from_millis(15_500));
    }

    #[test]
    fn urls_never_reach_a_log() {
        assert_eq!(scrub(KEYED_ERROR), "request failed for <url> (timeout)");
        assert_eq!(scrub("wss://a.b/key, then http://c/d"), "<url>, then <url>");
        assert_eq!(scrub(NOT_FOUND), NOT_FOUND);
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn lines(&self) -> Vec<String> {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes).unwrap().lines().map(str::to_string).collect()
        }
    }

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    async fn logged(
        rpc: &ScriptedRpc,
        tx: &RelayTransaction,
        receipt: &TransactionReceipt,
    ) -> Vec<String> {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(captured.clone())
            .with_max_level(tracing::Level::INFO)
            .finish();
        run(rpc, tx, receipt).with_subscriber(subscriber).await;
        captured.lines()
    }

    fn assert_identifies(line: &str, step: &str, attempt: u32) {
        for field in [
            format!("step={step}"),
            format!("tx_hash={TX}"),
            format!("chain_id={CHAIN}"),
            format!("attempt={attempt}"),
            "bundle_id=Some(".to_string(),
            "elapsed_ms=".to_string(),
        ] {
            assert!(line.contains(&field), "{field} missing from: {line}");
        }
    }

    #[tokio::test]
    async fn a_recovered_trace_logs_each_failure_and_one_info() {
        let rpc = ScriptedRpc::default();
        rpc.fail(TRACE, 2, NOT_FOUND);
        rpc.reply(TRACE, Ok(swap_trace()));
        let tx = landed(intent(vec![swap_call()]));

        let lines = logged(&rpc, &tx, &receipt(vec![])).await;

        assert_eq!(lines.len(), 3, "{lines:#?}");
        for (i, line) in lines[..2].iter().enumerate() {
            assert!(line.contains("WARN"), "{line}");
            assert!(line.contains(NOT_FOUND), "{line}");
            assert_identifies(line, "Trace", i as u32 + 1);
        }
        assert!(lines[2].contains("INFO"), "{}", lines[2]);
        assert_identifies(&lines[2], "Trace", 3);
    }

    #[tokio::test]
    async fn a_first_try_success_logs_nothing() {
        let rpc = ScriptedRpc::default();
        rpc.reply(TRACE, Ok(swap_trace()));
        let tx = landed(intent(vec![swap_call()]));

        assert_eq!(logged(&rpc, &tx, &receipt(vec![])).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_fallback_logs_the_give_up_and_the_receipt_step() {
        let rpc = ScriptedRpc::default();
        rpc.fail(TRACE, 6, KEYED_ERROR);
        let tx = landed(intent(vec![swap_call()]));

        let lines = logged(&rpc, &tx, &receipt(vec![])).await;

        assert_eq!(lines.len(), 7, "{lines:#?}");
        assert!(lines[5].contains("ERROR"), "{}", lines[5]);
        assert_identifies(&lines[5], "Trace", 6);
        assert!(lines[6].contains("WARN"), "{}", lines[6]);
        assert_identifies(&lines[6], "Receipt", 1);
        assert!(lines.iter().all(|l| !l.contains("secret-key") && !l.contains("node.example")));
    }

    #[tokio::test]
    async fn a_metadata_failure_logs_its_step() {
        let rpc = ScriptedRpc::default();
        rpc.fail("eth_chainId", 6, "header not found");
        let tx = landed(intent(vec![transfer_call(FRIEND, 5)]));
        let receipt = receipt(vec![transfer_log(USDC, EOA, FRIEND, 5)]);

        let lines = logged(&rpc, &tx, &receipt).await;

        assert_eq!(lines.len(), 6, "{lines:#?}");
        assert!(lines[5].contains("ERROR") && lines[5].contains("header not found"));
        assert_identifies(&lines[5], "Metadata", 6);
    }

    #[tokio::test]
    async fn unreadable_calls_log_the_classify_step_and_trace() {
        let rpc = ScriptedRpc::default();
        rpc.reply(TRACE, Ok(swap_trace()));
        let garbage =
            IntentV05 { eoa: EOA, executionData: bytes!("0xdeadbeef"), ..Default::default() };
        let tx = landed(garbage);

        let lines = logged(&rpc, &tx, &receipt(vec![])).await;

        assert_eq!(rpc.asked(TRACE), 1);
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(lines[0].contains("WARN"), "{}", lines[0]);
        assert_identifies(&lines[0], "Classify", 1);
    }
}
