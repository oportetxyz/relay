use super::invalid_params;
use crate::types::KeyHash;
use thiserror::Error;

/// Errors related to authorization keys.
#[derive(Debug, Error)]
pub enum KeysError {
    /// The key type is not supported.
    #[error("only supports `p256`, `webauthnp256` and `secp256k1` key types")]
    UnsupportedKeyType,
    /// Precalls are only allowed to modify one key to ensure correct ordering.
    #[error("precall can't modify multiple keys")]
    PrecallConflictingKeys,
    /// Should only have admin authorization keys.
    #[error("should only have admin authorization keys")]
    OnlyAdminKeyAllowed,
    /// Invalid signature.
    #[error("invalid signature")]
    InvalidSignature,
    /// Unknown key hash.
    #[error("key hash {0} is unknown")]
    UnknownKeyHash(KeyHash),
    /// A key's metadata field is malformed or too long.
    #[error("invalid key metadata: {0}")]
    InvalidMetadata(&'static str),
}

impl From<KeysError> for jsonrpsee::types::error::ErrorObject<'static> {
    fn from(err: KeysError) -> Self {
        match err {
            KeysError::UnsupportedKeyType
            | KeysError::PrecallConflictingKeys
            | KeysError::OnlyAdminKeyAllowed
            | KeysError::InvalidSignature
            | KeysError::UnknownKeyHash { .. }
            | KeysError::InvalidMetadata(_) => invalid_params(err.to_string()),
        }
    }
}
