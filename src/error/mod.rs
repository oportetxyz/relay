//! Relay error types.
mod asset;
use std::error::Error;

pub use asset::AssetError;

mod auth;
pub use auth::AuthError;

mod contracts;
pub use contracts::ContractErrors;

mod email;
pub use email::EmailError;

mod phone;
pub use phone::PhoneError;

mod onramp;
pub use onramp::OnrampError;

mod keys;
pub use keys::KeysError;

mod intent;
pub use intent::IntentError;

mod merkle;
pub use merkle::MerkleError;

mod quote;
pub use quote::QuoteError;

mod storage;
pub use storage::StorageError;

use alloy::{
    primitives::{Address, Bytes, ChainId},
    providers::MulticallError,
    transports::TransportErrorKind,
};
use thiserror::Error;

/// The relay overarching error type.
#[derive(Debug, Error)]
pub enum RelayError {
    /// Errors related to assets.
    #[error(transparent)]
    Asset(#[from] AssetError),
    /// Errors related to 7702 authorizations.
    #[error(transparent)]
    Auth(#[from] Box<AuthError>),
    /// Errors related to quotes.
    #[error(transparent)]
    Quote(#[from] QuoteError),
    /// Errors related to intents.
    #[error(transparent)]
    Intent(Box<IntentError>),
    /// Errors related to authorization keys.
    #[error(transparent)]
    Keys(#[from] KeysError),
    /// Errors related to storage.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The chain is not supported.
    #[error("unsupported chain {0}")]
    UnsupportedChain(ChainId),
    /// `wallet_ethCall` was asked to read a contract that is not on the
    /// allowlist. Deliberately says which pair was refused: the allowlist is
    /// public in the source, so naming it leaks nothing and saves the caller
    /// guessing why an otherwise valid read returned an error.
    #[error("reads are not allowed for {to} on chain {chain}")]
    ReadNotAllowed {
        /// The chain the caller asked to read on.
        chain: ChainId,
        /// The contract the caller asked to read.
        to: Address,
    },
    /// `wallet_ethCall` was called without a verified identity. The method is
    /// authenticated because it spends our provider quota on the caller's
    /// behalf.
    #[error("this method requires an authenticated session")]
    ReadRequiresAuth,
    /// `wallet_sponsorshipQuota` under `quota_key = address` was called
    /// without the `account` it should report on.
    #[error("sponsorship quota on this chain is per account: pass `account`")]
    QuotaNeedsAccount,
    /// `wallet_executePreCalls` found nothing to land: no precall is stored for
    /// the account on that chain, or every stored one is already consumed.
    #[error("no stored precalls for {eoa} on chain {chain}")]
    NoStoredPreCalls {
        /// The account.
        eoa: Address,
        /// The chain.
        chain: ChainId,
    },
    /// `wallet_executePreCalls` is paid by the relay, and the sponsorship
    /// policy declined to pay for this caller.
    #[error("executing precalls for {eoa} on chain {chain} is not sponsored")]
    PreCallsNotSponsored {
        /// The account.
        eoa: Address,
        /// The chain.
        chain: ChainId,
    },
    /// The session token does not list the address a sponsored request is for.
    #[error("{eoa} is not in the session token")]
    AddressNotInToken {
        /// The account.
        eoa: Address,
    },
    /// The orchestrator is not supported.
    #[error("unsupported orchestrator {0}")]
    UnsupportedOrchestrator(Address),
    /// The asset is not supported.
    #[error("unsupported asset {asset} on chain {chain}")]
    UnsupportedAsset {
        /// The address of the asset that is not supported.
        asset: Address,
        /// The chain ID where the asset is not supported.
        chain: ChainId,
    },
    /// An error occurred during ABI encoding/decoding.
    #[error(transparent)]
    AbiError(#[from] alloy::sol_types::Error),
    /// An error occurred talking to RPC.
    #[error(transparent)]
    RpcError(#[from] alloy::transports::RpcError<TransportErrorKind>),
    /// Contract error.
    #[error(transparent)]
    ContractError(#[from] alloy::contract::Error),
    /// The relay is unhealthy.
    #[error("service is unhealthy")]
    Unhealthy,
    /// The relay is unhealthy with detailed report.
    #[error("service is unhealthy: db_healthy={is_db_ok}, unhealthy_chains=[{unhealthy_chains:?}]")]
    UnhealthyReport {
        /// Whether the database is healthy.
        is_db_ok: bool,
        /// List of unhealthy chain IDs.
        unhealthy_chains: Vec<ChainId>,
    },
    /// An internal error occurred.
    #[error(transparent)]
    InternalError(#[from] eyre::Error),
    /// Settlement-related errors.
    #[error(transparent)]
    Settlement(#[from] crate::interop::SettlementError),
}

impl RelayError {
    /// Creates an [`RelayError::InternalError`] from an error.
    pub fn internal(err: impl Error + Send + Sync + 'static) -> Self {
        Self::InternalError(err.into())
    }

    /// Creates an [`RelayError::InternalError`] from a message.
    pub fn internal_msg(msg: impl Into<String>) -> Self {
        Self::InternalError(eyre::eyre!(msg.into()))
    }
}

impl From<reqwest::Error> for RelayError {
    fn from(err: reqwest::Error) -> Self {
        Self::InternalError(err.into())
    }
}

impl From<IntentError> for RelayError {
    fn from(err: IntentError) -> Self {
        Self::Intent(Box::new(err))
    }
}

impl From<AuthError> for RelayError {
    fn from(err: AuthError) -> Self {
        Self::Auth(err.boxed())
    }
}

impl From<MulticallError> for RelayError {
    fn from(err: MulticallError) -> Self {
        match err {
            MulticallError::TransportError(err) => Self::RpcError(err),
            MulticallError::DecodeError(err) => Self::AbiError(err),
            _ => Self::InternalError(err.into()),
        }
    }
}

impl From<RelayError> for jsonrpsee::types::error::ErrorObject<'static> {
    fn from(err: RelayError) -> Self {
        match err {
            RelayError::Asset(inner) => inner.into(),
            RelayError::Auth(inner) => (*inner).into(),
            RelayError::Quote(inner) => inner.into(),
            RelayError::Intent(inner) => (*inner).into(),
            RelayError::Keys(inner) => inner.into(),
            RelayError::Storage(inner) => inner.into(),
            // Caller errors, not ours. Deliberately NOT internal_rpc: an
            // internal code tells a client "our fault, retry", and retrying an
            // unlisted contract or an anonymous read never succeeds.
            RelayError::ReadNotAllowed { .. }
            | RelayError::QuotaNeedsAccount
            | RelayError::NoStoredPreCalls { .. }
            | RelayError::PreCallsNotSponsored { .. }
            | RelayError::UnsupportedChain(_)
            | RelayError::UnsupportedOrchestrator(_)
            | RelayError::UnsupportedAsset { .. } => invalid_params(err.to_string()),
            RelayError::AddressNotInToken { .. } => {
                rpc_err(ADDRESS_NOT_IN_TOKEN_CODE, err.to_string(), None)
            }
            RelayError::ReadRequiresAuth => {
                rpc_err(jsonrpsee::types::error::INVALID_REQUEST_CODE, err.to_string(), None)
            }
            // The chain node failed, after the provider's own retries. viem retries -32603 on
            // every layer of a client's transport stack, so each client retry would fan out into
            // more node calls, the worst case being a rate-limited node. EIP-1474's
            // "resource unavailable" is not retried.
            RelayError::RpcError(_) => rpc_err(RESOURCE_UNAVAILABLE_CODE, err.to_string(), None),
            RelayError::AbiError(_)
            | RelayError::ContractError(_)
            | RelayError::Unhealthy
            | RelayError::UnhealthyReport { .. }
            | RelayError::InternalError(_)
            | RelayError::Settlement(_) => internal_rpc(err.to_string()),
        }
    }
}

/// EIP-1474 "resource unavailable" error code.
const RESOURCE_UNAVAILABLE_CODE: i32 = -32002;

/// A sponsored request for an address its token does not list. Clients refresh the token and
/// retry once on this code.
const ADDRESS_NOT_IN_TOKEN_CODE: i32 = -32090;

/// Constructs an invalid params JSON‑RPC error.
fn invalid_params(msg: impl Into<String>) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(jsonrpsee::types::error::INVALID_PARAMS_CODE, msg, None)
}

/// Constructs an internal JSON‑RPC error.
fn internal_rpc(msg: impl Into<String>) -> jsonrpsee::types::error::ErrorObject<'static> {
    rpc_err(jsonrpsee::types::error::INTERNAL_ERROR_CODE, msg, None)
}

/// Constructs a JSON‑RPC error with `code`, `message` and optional `data`.
fn rpc_err(
    code: i32,
    msg: impl Into<String>,
    data: Option<Bytes>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    jsonrpsee::types::error::ErrorObject::owned(code, msg.into(), data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::types::error::{ErrorObject, INVALID_PARAMS_CODE};

    #[test]
    fn deterministic_and_node_errors_use_codes_viem_does_not_retry() {
        let code = |err: RelayError| ErrorObject::from(err).code();

        assert_eq!(code(RelayError::UnsupportedChain(1)), INVALID_PARAMS_CODE);
        assert_eq!(
            code(RelayError::RpcError(TransportErrorKind::custom_str("429 Too Many Requests"))),
            RESOURCE_UNAVAILABLE_CODE
        );
    }
}
