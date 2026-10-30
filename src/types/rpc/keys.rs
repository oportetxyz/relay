//! RPC key-related request and response types.

use alloy::{
    primitives::{Address, B256, Bytes, ChainId, U64, keccak256},
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::KeysError,
    types::{Call, Key, KeyHash, KeyType},
};

use super::Permission;
use std::collections::HashMap;

/// Request parameters for `wallet_getKeys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetKeysParameters {
    /// Address of the account to get the keys for.
    pub address: Address,
    /// Target chain IDs. If not provided, returns keys for all supported chains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(with = "alloy::serde::quantity::vec")]
    pub chain_ids: Vec<ChainId>,
}

/// Response for wallet_getKeys with multi-chain support.
/// Maps chain ID (as QUANTITY hex string per JSON-RPC spec) to array of authorized keys for that
/// chain. Chains where the account is not delegated or where errors occurred are omitted.
pub type GetKeysResponse = HashMap<U64, Vec<AuthorizeKeyResponse>>;

/// Represents a key authorization request.
///
/// If the key does not exist, it is added to the account, along with the permissions.
///
/// If the key already exists, the permissions are updated.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizeKey {
    /// The key to authorize or modify permissions for.
    #[serde(flatten)]
    pub key: Key,
    /// The permissions for the key.
    pub permissions: Vec<Permission>,
    /// What the client knows about the WebAuthn credential behind the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<KeyMetadata>,
}

impl AuthorizeKey {
    /// Transform this key authorization into a series of calls.
    ///
    /// The first call is to add the key to the account. It should only be part of the final
    /// [`Intent`] if the key does not already exist.
    ///
    /// The second set of calls is to add call permissions and spending limits to the key.
    ///
    /// The third set of calls is to register the mapping from key id to account address in a
    /// registry.
    pub fn into_calls(mut self) -> Result<(Call, Vec<Call>), KeysError> {
        let mut calls = Vec::new();

        calls.extend(self.permissions.drain(..).map(|perm| match perm {
            Permission::Call(perm) => {
                Call::set_can_execute(self.key.key_hash(), perm.to, perm.selector, true)
            }
            Permission::Spend(perm) => {
                Call::set_spend_limit(self.key.key_hash(), perm.token, perm.period, perm.limit)
            }
        }));

        Ok((Call::authorize(self.key), calls))
    }

    /// Returns the inner [`KeyType`].
    pub fn key_type(&self) -> KeyType {
        self.key.keyType
    }

    /// Convert `self` into [`AuthorizeKeyResponse`].
    pub fn into_response(self) -> AuthorizeKeyResponse {
        AuthorizeKeyResponse { hash: self.key.key_hash(), authorize_key: self }
    }

    /// Returns the metadata to store for this key, if it is a WebAuthn key that carries any.
    ///
    /// Metadata on other key types is dropped, since only a WebAuthn credential has an
    /// attachment or transports.
    pub fn metadata_entry(&self) -> Result<Option<KeyMetadataEntry>, KeysError> {
        let Some(metadata) = &self.metadata else { return Ok(None) };
        if !self.key.keyType.is_webauthn() {
            return Ok(None);
        }
        metadata.validate()?;
        Ok(Some(KeyMetadataEntry { key_hash: self.key.key_hash(), metadata: metadata.clone() }))
    }
}

/// Returns the metadata of `keys` to store once the bundle authorizing them is accepted.
pub fn metadata_entries<'a>(
    keys: impl IntoIterator<Item = &'a AuthorizeKey>,
) -> Result<Vec<KeyMetadataEntry>, KeysError> {
    keys.into_iter().filter_map(|key| key.metadata_entry().transpose()).collect()
}

/// How a WebAuthn credential is attached to the device that created it.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuthenticatorAttachment {
    /// Built into the device, synced through the platform's password manager.
    Platform,
    /// A roaming authenticator, such as a USB or NFC security key.
    CrossPlatform,
}

impl AuthenticatorAttachment {
    /// The WebAuthn spelling, as stored.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::CrossPlatform => "cross-platform",
        }
    }

    /// Parses the WebAuthn spelling. Unknown values read as `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "platform" => Some(Self::Platform),
            "cross-platform" => Some(Self::CrossPlatform),
            _ => None,
        }
    }
}

/// What the client reported about a WebAuthn credential when it was created.
///
/// It is a hint for the client's next sign-in (which authenticators to offer), never an input to
/// signature checks.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeyMetadata {
    /// The credential ID, base64url without padding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// The credential's authenticator attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AuthenticatorAttachment>,
    /// The transports the authenticator reported, such as `internal`, `hybrid` or `usb`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transports: Vec<String>,
}

impl KeyMetadata {
    /// WebAuthn caps a credential ID at 1023 bytes, which is 1364 base64url characters.
    pub const MAX_CREDENTIAL_ID_LEN: usize = 1364;
    /// More transports than WebAuthn defines, with room for new ones.
    pub const MAX_TRANSPORTS: usize = 8;
    /// The longest transport WebAuthn defines is `smart-card`.
    pub const MAX_TRANSPORT_LEN: usize = 32;

    /// A digest of the metadata, so a relay-signed quote can cover it.
    pub fn digest(&self) -> B256 {
        keccak256(
            (
                self.credential_id.clone().unwrap_or_default(),
                self.attachment
                    .map(|attachment| attachment.as_str())
                    .unwrap_or_default()
                    .to_string(),
                self.transports.clone(),
            )
                .abi_encode(),
        )
    }

    /// Checks sizes and alphabets, so a client can't store arbitrary data under a key.
    pub fn validate(&self) -> Result<(), KeysError> {
        let is_base64url = |value: &str| {
            value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        if let Some(id) = &self.credential_id
            && (id.is_empty() || id.len() > Self::MAX_CREDENTIAL_ID_LEN || !is_base64url(id))
        {
            return Err(KeysError::InvalidMetadata("credentialId"));
        }
        if self.transports.len() > Self::MAX_TRANSPORTS
            || self.transports.iter().any(|transport| {
                transport.is_empty()
                    || transport.len() > Self::MAX_TRANSPORT_LEN
                    || !transport.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
            })
        {
            return Err(KeysError::InvalidMetadata("transports"));
        }
        Ok(())
    }
}

/// [`KeyMetadata`] for one key, carried from the prepare step to the step that stores it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeyMetadataEntry {
    /// The hash of the key the metadata describes.
    pub key_hash: KeyHash,
    /// The metadata.
    #[serde(flatten)]
    pub metadata: KeyMetadata,
}

/// Represents a key authorization response.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct AuthorizeKeyResponse {
    /// Key hash.
    pub hash: B256,
    /// The key to authorize or modify permissions for.
    #[serde(flatten)]
    pub authorize_key: AuthorizeKey,
}

/// Represents a key revocation request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokeKey {
    /// Key hash to revoke.
    pub hash: B256,
}

impl RevokeKey {
    /// Transform into a series of calls.
    pub fn into_calls(self) -> Vec<Call> {
        vec![Call::revoke(self.hash)]
    }
}

/// Key Signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeySignature {
    /// Public key that generated the signature.
    pub public_key: Bytes,
    /// Type of key that generated the signature.
    #[serde(rename = "type")]
    pub key_type: KeyType,
    /// Signature value.
    pub value: Bytes,
    /// Whether it should prehash before verifying the signature.
    #[serde(default)]
    pub prehash: bool,
}

impl KeySignature {
    /// Returns the associated [`KeyHash`].
    pub fn key_hash(&self) -> KeyHash {
        Key::hash(self.key_type, &self.public_key)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, Bytes, U256, fixed_bytes};

    use crate::types::{
        Call, CallPermission,
        IthacaAccount::SpendPeriod,
        Key, KeyType, U40,
        rpc::{
            AuthenticatorAttachment, AuthorizeKey, AuthorizeKeyResponse, KeyMetadata, Permission,
            RevokeKey, SpendPermission, metadata_entries,
        },
    };

    fn webauthn_key(metadata: Option<KeyMetadata>) -> AuthorizeKey {
        AuthorizeKey {
            key: Key {
                expiry: U40::from(0),
                keyType: KeyType::WebAuthnP256,
                isSuperAdmin: true,
                publicKey: Bytes::from(vec![7; 64]),
            },
            permissions: vec![],
            metadata,
        }
    }

    fn security_key_metadata() -> KeyMetadata {
        KeyMetadata {
            credential_id: Some("AbC-_09".to_string()),
            attachment: Some(AuthenticatorAttachment::CrossPlatform),
            transports: vec!["usb".to_string(), "nfc".to_string()],
        }
    }

    #[test]
    fn key_metadata_round_trips_and_stays_optional() {
        let with = webauthn_key(Some(security_key_metadata()));
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(
            json["metadata"],
            serde_json::json!({
                "credentialId": "AbC-_09",
                "attachment": "cross-platform",
                "transports": ["usb", "nfc"],
            })
        );
        assert_eq!(serde_json::from_str::<AuthorizeKey>(&json.to_string()).unwrap(), with);

        // Keys from clients that send no metadata serialize exactly as before.
        let without = webauthn_key(None);
        let json = serde_json::to_value(&without).unwrap();
        assert!(json.get("metadata").is_none());
        assert_eq!(serde_json::from_str::<AuthorizeKey>(&json.to_string()).unwrap(), without);
    }

    #[test]
    fn key_metadata_validation() {
        assert!(security_key_metadata().validate().is_ok());
        assert!(KeyMetadata::default().validate().is_ok());

        let bad = [
            KeyMetadata { credential_id: Some(String::new()), ..Default::default() },
            KeyMetadata { credential_id: Some("a+b/c=".to_string()), ..Default::default() },
            KeyMetadata {
                credential_id: Some("a".repeat(KeyMetadata::MAX_CREDENTIAL_ID_LEN + 1)),
                ..Default::default()
            },
            KeyMetadata { transports: vec!["USB".to_string()], ..Default::default() },
            KeyMetadata { transports: vec![String::new()], ..Default::default() },
            KeyMetadata {
                transports: vec!["usb".to_string(); KeyMetadata::MAX_TRANSPORTS + 1],
                ..Default::default()
            },
        ];
        for metadata in bad {
            assert!(metadata.validate().is_err(), "{metadata:?}");
        }
    }

    #[test]
    fn metadata_entries_keep_only_webauthn_keys() {
        let mut p256 = webauthn_key(Some(security_key_metadata()));
        p256.key.keyType = KeyType::P256;
        let webauthn = webauthn_key(Some(security_key_metadata()));

        let entries = metadata_entries([&p256, &webauthn_key(None), &webauthn]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key_hash, webauthn.key.key_hash());
        assert_eq!(entries[0].metadata, security_key_metadata());

        let invalid = webauthn_key(Some(KeyMetadata {
            transports: vec!["usb usb".to_string()],
            ..Default::default()
        }));
        assert!(metadata_entries([&invalid]).is_err());
    }

    #[test]
    fn key_metadata_digest_covers_every_field() {
        let base = security_key_metadata();
        let changed = [
            KeyMetadata { credential_id: None, ..base.clone() },
            KeyMetadata { attachment: Some(AuthenticatorAttachment::Platform), ..base.clone() },
            KeyMetadata { transports: vec!["usb".to_string()], ..base.clone() },
        ];
        for other in changed {
            assert_ne!(base.digest(), other.digest(), "{other:?}");
        }
    }

    #[test]
    fn test_authorize_into_calls() {
        let key = AuthorizeKey {
            key: Key {
                expiry: U40::from(0),
                keyType: KeyType::P256,
                isSuperAdmin: false,
                publicKey: Bytes::from(fixed_bytes!(
                    "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                )),
            },
            permissions: vec![
                Permission::Call(CallPermission {
                    to: Address::ZERO,
                    selector: fixed_bytes!("0xa9059cbb"),
                }),
                Permission::Spend(SpendPermission {
                    limit: U256::from(1000),
                    period: SpendPeriod::Day,
                    token: Address::ZERO,
                }),
            ],
            metadata: None,
        };

        let (authorize, calls) = key.clone().into_calls().unwrap();

        assert_eq!(authorize, Call::authorize(key.clone().key));
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            Call::set_can_execute(
                key.key.key_hash(),
                Address::ZERO,
                fixed_bytes!("0xa9059cbb"),
                true
            )
        );
        assert_eq!(
            calls[1],
            Call::set_spend_limit(
                key.key.key_hash(),
                Address::ZERO,
                SpendPeriod::Day,
                U256::from(1000)
            )
        );
    }

    #[test]
    fn serialize_authorize_key() {
        let key = AuthorizeKey {
            key: Key {
                expiry: U40::from(0),
                keyType: KeyType::P256,
                isSuperAdmin: false,
                publicKey: Bytes::from(fixed_bytes!(
                    "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                )),
            },
            permissions: vec![
                Permission::Call(CallPermission {
                    to: Address::ZERO,
                    selector: fixed_bytes!("0xa9059cbb"),
                }),
                Permission::Spend(SpendPermission {
                    limit: U256::from(1000),
                    period: SpendPeriod::Day,
                    token: Address::ZERO,
                }),
            ],
            metadata: None,
        };

        assert_eq!(
            serde_json::to_string(&key).unwrap(),
            r#"{"expiry":"0x0","type":"p256","role":"normal","publicKey":"0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef","permissions":[{"type":"call","selector":"0xa9059cbb","to":"0x0000000000000000000000000000000000000000"},{"type":"spend","limit":"0x3e8","period":"day","token":"0x0000000000000000000000000000000000000000"}]}"#
        );
    }

    #[test]
    fn serialize_authorize_key_response() {
        let key = Key {
            expiry: U40::from(0),
            keyType: KeyType::P256,
            isSuperAdmin: false,
            publicKey: Bytes::from(fixed_bytes!(
                "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            )),
        };
        let resp = AuthorizeKeyResponse {
            hash: key.key_hash(),
            authorize_key: AuthorizeKey {
                key,
                permissions: vec![Permission::Call(CallPermission {
                    to: Address::ZERO,
                    selector: fixed_bytes!("0xa9059cbb"),
                })],
                metadata: None,
            },
        };

        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"hash":"0xc7982d8475577e50ca7dc56923eb413813cdb93f009160d943436b217410ffd9","expiry":"0x0","type":"p256","role":"normal","publicKey":"0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef","permissions":[{"type":"call","selector":"0xa9059cbb","to":"0x0000000000000000000000000000000000000000"}]}"#
        );
    }

    #[test]
    fn deserialize_authorize_key_response() {
        let resp = serde_json::from_str::<AuthorizeKeyResponse>(
            r#"{"hash":"0xc7982d8475577e50ca7dc56923eb413813cdb93f009160d943436b217410ffd9","expiry":"0x0","type":"p256","role":"normal","publicKey":"0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef","permissions":[{"type":"call","selector":"0xa9059cbb","to":"0x0000000000000000000000000000000000000000"}]}"#
        ).unwrap();

        let key = Key {
            expiry: U40::from(0),
            keyType: KeyType::P256,
            isSuperAdmin: false,
            publicKey: Bytes::from(fixed_bytes!(
                "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            )),
        };

        assert_eq!(
            resp,
            AuthorizeKeyResponse {
                hash: key.key_hash(),
                authorize_key: AuthorizeKey {
                    key,
                    permissions: vec![Permission::Call(CallPermission {
                        to: Address::ZERO,
                        selector: fixed_bytes!("0xa9059cbb"),
                    })],
                    metadata: None,
                },
            }
        );
    }

    #[test]
    fn test_revoke_key_into_calls() {
        let hash = B256::random();

        let revoke = RevokeKey { hash };
        assert_eq!(revoke.into_calls(), vec![Call::revoke(hash)]);
    }

    #[test]
    fn test_get_keys_parameters_roundtrip() {
        // Test with chain_ids specified
        let raw =
            r#"{"address":"0x0000000000000000000000000000000000000000","chainIds":["0x1","0xa"]}"#;
        let params =
            super::GetKeysParameters { address: Address::ZERO, chain_ids: vec![1u64, 10u64] };

        let json = serde_json::to_string(&params).unwrap();
        assert_eq!(json, raw);
        let deserialized: super::GetKeysParameters = serde_json::from_str(&json).unwrap();

        assert_eq!(params.address, deserialized.address);
        assert_eq!(params.chain_ids, deserialized.chain_ids);

        // Test with empty chain_ids (should omit field in JSON)
        let params_no_chains =
            super::GetKeysParameters { address: Address::ZERO, chain_ids: vec![] };

        let json_no_chains = serde_json::to_string(&params_no_chains).unwrap();
        assert_eq!(json_no_chains, r#"{"address":"0x0000000000000000000000000000000000000000"}"#);

        // Test deserialization with missing chain_ids (should default to empty vec)
        let deserialized_no_chains: super::GetKeysParameters =
            serde_json::from_str(r#"{"address":"0x0000000000000000000000000000000000000000"}"#)
                .unwrap();
        assert_eq!(deserialized_no_chains.chain_ids.len(), 0);
    }
}
