//! Quote types.

use crate::{
    error::{QuoteError, RelayError},
    types::{AssetDeficits, Intent, Intents, Signed, VersionedContracts, rpc::KeyMetadataEntry},
};
use alloy::{
    primitives::{Address, B256, ChainId, Keccak256, Sealable, Signature, U256},
    providers::utils::Eip1559Estimation,
    rpc::types::SignedAuthorization,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A relay-signed [`Quotes`].
pub type SignedQuotes = Signed<Quotes>;

/// Separates the fee payer quote from the rest of [`Quotes::digest`].
///
/// why: the other optional fields are hashed in 32 byte words, so a bare digest here could be
/// sent back as `multi_chain_root` and hash the same. This tag is not a multiple of 32 bytes.
const FEE_PAYER_QUOTE_TAG: &[u8] = b"relay.quotes.feePayerQuote";

/// A set of quotes from the relay with a set of intents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quotes {
    /// A quote for each intent.
    ///
    /// For a single-chain workflow, this will have exactly one item, the output intent.
    ///
    /// For a multi-chain workflow, this will have multiple items, where the last one is the output
    /// intent.
    pub quotes: Vec<Quote>,
    /// The time at which this estimate expires.
    ///
    /// This is a UNIX timestamp in seconds.
    #[serde(with = "crate::serde::timestamp")]
    pub ttl: SystemTime,
    /// Merkle root if it's a multichain
    pub multi_chain_root: Option<B256>,
    /// Optional quote for the fee payer.
    pub fee_payer_quote: Option<Box<Quote>>,
    /// Metadata of the WebAuthn keys the intents authorize, stored once the bundle is accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_metadata: Vec<KeyMetadataEntry>,
}

impl Quotes {
    /// Sets the merkle payload to every quote.
    pub fn with_merkle_payload(
        mut self,
        contracts: &VersionedContracts,
    ) -> Result<Self, RelayError> {
        let mut intents = Intents::new(
            self.quotes
                .iter()
                .map(|quote| {
                    Ok((
                        quote.intent.clone(),
                        contracts.get_versioned_orchestrator(quote.orchestrator)?.clone(),
                        quote.chain_id,
                    ))
                })
                .collect::<Result<Vec<_>, RelayError>>()?,
        );

        self.multi_chain_root = Some(intents.root()?);

        Ok(self)
    }

    /// Get the total fees in all of the inner quotes combined.
    ///
    /// For now, this assumes a uniform fee token across all intents, but we may change this in the
    /// future.
    ///
    /// Returns `None` if there are no inner quotes.
    pub fn fees(&self) -> Option<(Address, U256)> {
        Some((
            self.quotes.first()?.intent.payment_token(),
            self.quotes.iter().map(|quote| quote.intent.total_payment_max_amount()).sum(),
        ))
    }

    /// Add a signature turning the quotes into a [`SignedQuotes`].
    pub fn into_signed(self, signature: Signature) -> SignedQuotes {
        let digest = self.digest();
        SignedQuotes::new_unchecked(self, signature, digest)
    }

    /// Compute a digest of the quotes for signing.
    pub fn digest(&self) -> B256 {
        let mut hasher = Keccak256::new();
        for quote in &self.quotes {
            hasher.update(quote.digest());
        }
        hasher.update(
            self.ttl
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::from_secs(0))
                .as_secs()
                .to_be_bytes(),
        );
        if let Some(root) = self.multi_chain_root {
            hasher.update(root);
        }
        // why: puts the fee payer quote under the relay's signature. Nothing is added when there
        // is none, so quotes without one hash as before.
        if let Some(quote) = &self.fee_payer_quote {
            hasher.update(FEE_PAYER_QUOTE_TAG);
            hasher.update(quote.digest());
        }
        // why: puts the metadata under the relay's signature. Nothing is added when there is
        // none, so quotes without it hash as before.
        for entry in &self.key_metadata {
            hasher.update(entry.key_hash);
            hasher.update(entry.metadata.digest());
        }
        hasher.finalize()
    }
}

impl Sealable for Quotes {
    fn hash_slow(&self) -> B256 {
        self.digest()
    }
}

/// Which side of a bundle a [`Quote`] pays for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteRole {
    /// The user's own intent.
    User,
    /// The fee payer's intent, which only pays the fee.
    FeePayer,
}

impl QuoteRole {
    /// Refuses a quote this role must not send.
    ///
    /// why: the relay signs fund transfers with its funder key. They belong to a user's output
    /// intent, and a fee payer quote never carries any.
    pub fn check(self, quote: &Quote) -> Result<(), QuoteError> {
        if self == Self::FeePayer && !quote.intent.encoded_fund_transfers().is_empty() {
            return Err(QuoteError::FeePayerFundTransfers);
        }
        Ok(())
    }
}

/// A quote from a relay for a given [`Intent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    /// The chain ID.
    #[serde(with = "alloy::serde::quantity")]
    pub chain_id: ChainId,
    /// The intent.
    pub intent: Intent,
    /// Extra payment for e.g L1 DA fee that is paid on top of the execution gas.
    pub extra_payment: U256,
    /// Price of the ETH in the [`Intent::paymentToken`] in wei.
    pub eth_price: U256,
    /// Decimals of the [`Intent::paymentToken`].
    pub payment_token_decimals: u8,
    /// The recommended gas limit for the transaction.
    #[serde(with = "alloy::serde::quantity")]
    pub tx_gas: u64,
    /// The fee estimate for the action in the destination chains native token.
    pub native_fee_estimate: Eip1559Estimation,
    /// An optional unsigned authorization item.
    ///
    /// The account in `intent.eoa` will be delegated to this address.
    pub authorization_address: Option<Address>,
    /// An optional additional authorization address, which would be used to delegate the feepayer
    pub additional_authorization: Option<SignedAuthorization>,
    /// Orchestrator to use for the transaction.
    pub orchestrator: Address,
    /// How much of the fee token the fee payer (or user if no fee payer) is missing to pay for
    /// this intent.
    ///
    /// If it is 0, the fee payer has enough balance to execute the call.
    #[serde(default)]
    pub fee_token_deficit: U256,
    /// Assets user is missing for the intent to execute correctly.
    #[serde(default, skip_serializing_if = "AssetDeficits::is_empty")]
    pub asset_deficits: AssetDeficits,
}

impl Quote {
    /// Returns true if the quote has deficits.
    pub fn has_deficits(&self) -> bool {
        !self.asset_deficits.is_empty() || !self.fee_token_deficit.is_zero()
    }

    /// Compute a digest of the quote for signing.
    pub fn digest(&self) -> B256 {
        let Self {
            chain_id,
            intent,
            extra_payment,
            eth_price,
            payment_token_decimals,
            tx_gas,
            native_fee_estimate,
            authorization_address,
            additional_authorization,
            orchestrator,
            fee_token_deficit,
            asset_deficits,
        } = self;

        let mut hasher = Keccak256::new();
        hasher.update(chain_id.to_be_bytes());
        hasher.update(intent.digest());
        hasher.update(extra_payment.to_be_bytes::<32>());
        hasher.update(eth_price.to_be_bytes::<32>());
        hasher.update([*payment_token_decimals]);
        hasher.update(tx_gas.to_be_bytes());
        hasher.update(native_fee_estimate.max_fee_per_gas.to_be_bytes());
        hasher.update(native_fee_estimate.max_priority_fee_per_gas.to_be_bytes());
        if let Some(address) = authorization_address {
            hasher.update(address);
        }
        if let Some(auth) = additional_authorization {
            hasher.update(auth.signature_hash());
        }
        hasher.update(orchestrator);
        hasher.update([(!fee_token_deficit.is_zero() || !asset_deficits.is_empty()) as u8]);
        hasher.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::{
        primitives::{Bytes, b256},
        signers::{SignerSync, local::PrivateKeySigner},
    };

    /// Test that ensures all fields in Quote affect the digest.
    ///
    /// This test uses exhaustive struct initialization which will cause a compile error
    /// if a new field is added to Quote but not included in the test, forcing developers
    /// to update both the digest method and this test.
    #[test]
    fn test_quote_digest_includes_all_fields() {
        // Helper macro to test that modifying a field changes the digest
        macro_rules! test_field {
            ($base:expr, $field:ident, $new_value:expr, $msg:expr) => {{
                let mut modified = $base.clone();
                modified.$field = $new_value;
                assert_ne!($base.digest(), modified.digest(), $msg);
            }};
        }

        // Base quote - using exhaustive initialization (no `..Default::default()`)
        // This will fail to compile if a new field is added
        let base_quote = Quote {
            chain_id: 1,
            intent: Intent::default(),
            extra_payment: U256::from(100),
            eth_price: U256::from(2000),
            payment_token_decimals: 18,
            tx_gas: 21000,
            native_fee_estimate: Eip1559Estimation {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
            },
            authorization_address: Some(Address::ZERO),
            additional_authorization: None,
            orchestrator: Address::ZERO,
            fee_token_deficit: U256::ZERO,
            asset_deficits: AssetDeficits::default(),
        };

        test_field!(base_quote, chain_id, 2, "chain_id must affect digest");
        test_field!(base_quote, extra_payment, U256::from(200), "extra_payment must affect digest");
        test_field!(base_quote, eth_price, U256::from(3000), "eth_price must affect digest");
        test_field!(
            base_quote,
            payment_token_decimals,
            6,
            "payment_token_decimals must affect digest"
        );
        test_field!(base_quote, tx_gas, 50000, "tx_gas must affect digest");

        let mut modified = base_quote.clone();
        modified.native_fee_estimate.max_fee_per_gas = 200;
        assert_ne!(
            base_quote.digest(),
            modified.digest(),
            "native_fee_estimate.max_fee_per_gas must affect digest"
        );

        let mut modified = base_quote.clone();
        modified.native_fee_estimate.max_priority_fee_per_gas = 20;
        assert_ne!(
            base_quote.digest(),
            modified.digest(),
            "native_fee_estimate.max_priority_fee_per_gas must affect digest"
        );

        test_field!(
            base_quote,
            authorization_address,
            Some(Address::repeat_byte(1)),
            "authorization_address must affect digest"
        );

        test_field!(
            base_quote,
            orchestrator,
            Address::repeat_byte(1),
            "orchestrator must affect digest"
        );

        test_field!(
            base_quote,
            fee_token_deficit,
            U256::from(100),
            "fee_token_deficit must affect digest"
        );

        // Verify intent changes affect digest
        let mut modified = base_quote.clone();
        modified.intent = modified.intent.with_eoa(Address::repeat_byte(1));
        assert_ne!(base_quote.digest(), modified.digest(), "intent must affect digest");
    }

    /// A quote with every field fixed, so its digest can be pinned.
    fn fixed_quote(payment: u64) -> Quote {
        Quote {
            chain_id: 8453,
            intent: Intent::default()
                .with_eoa(Address::repeat_byte(1))
                .with_payer(Address::ZERO)
                .with_total_payment_max_amount(U256::from(payment)),
            extra_payment: U256::from(100),
            eth_price: U256::from(2000),
            payment_token_decimals: 6,
            tx_gas: 21000,
            native_fee_estimate: Eip1559Estimation {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
            },
            authorization_address: None,
            additional_authorization: None,
            orchestrator: Address::repeat_byte(2),
            fee_token_deficit: U256::ZERO,
            asset_deficits: AssetDeficits::default(),
        }
    }

    fn fixed_quotes(payment: u64, multi_chain_root: Option<B256>) -> Quotes {
        Quotes {
            quotes: vec![fixed_quote(payment)],
            ttl: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            multi_chain_root,
            fee_payer_quote: None,
            key_metadata: vec![],
        }
    }

    /// Digests taken from main at b92b1bab, before the fee payer quote was hashed. A change here
    /// invalidates every quote in flight, sponsored ones included.
    #[test]
    fn quotes_digest_without_fee_payer_is_pinned() {
        assert_eq!(
            fixed_quotes(300, None).digest(),
            b256!("0xb202836fa188bee9caf8ad9878dad1e4d07ee135b96d233f7e017b1263eb8d74"),
            "unsponsored"
        );
        assert_eq!(
            fixed_quotes(0, None).digest(),
            b256!("0xdac902af7aab29624c74333910b0dfcecdad6885887f5149e9fb57b1029f32b0"),
            "sponsored"
        );
        assert_eq!(
            fixed_quotes(0, Some(B256::repeat_byte(3))).digest(),
            b256!("0x805746c411c61d0aeed6e1c0674bacc504d663c68b148f0ec8adaba2b5d6eff1"),
            "sponsored multichain"
        );
    }

    fn sign(quotes: Quotes) -> (Address, serde_json::Value) {
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(7)).unwrap();
        let signature = signer.sign_hash_sync(&quotes.digest()).unwrap();
        (signer.address(), serde_json::to_value(quotes.into_signed(signature)).unwrap())
    }

    /// The signer recovered from a quote as the relay reads it on send.
    fn recover(wire: serde_json::Value) -> Address {
        serde_json::from_value::<SignedQuotes>(wire).unwrap().recover_address().unwrap()
    }

    #[test]
    fn quotes_without_fee_payer_verify_with_null_or_absent_field() {
        let (signer, wire) = sign(fixed_quotes(0, None));
        assert!(wire["feePayerQuote"].is_null());
        assert_eq!(recover(wire.clone()), signer);

        let mut absent = wire;
        absent.as_object_mut().unwrap().remove("feePayerQuote");
        assert_eq!(recover(absent), signer);
    }

    #[test]
    fn fee_payer_quote_is_bound_to_the_signature() {
        let fee_payer_quote = fixed_quote(300);
        let quotes = Quotes {
            fee_payer_quote: Some(Box::new(fee_payer_quote.clone())),
            ..fixed_quotes(0, None)
        };
        let (signer, wire) = sign(quotes);
        assert_eq!(recover(wire.clone()), signer, "the quote as signed verifies");

        let mut stripped = wire.clone();
        stripped.as_object_mut().unwrap().remove("feePayerQuote");
        assert_ne!(recover(stripped), signer, "stripped");

        let mut nulled = wire.clone();
        nulled["feePayerQuote"] = serde_json::Value::Null;
        assert_ne!(recover(nulled), signer, "set to null");

        let mut swapped = wire.clone();
        swapped["feePayerQuote"]["txGas"] = "0x1".into();
        assert_ne!(recover(swapped), signer, "swapped");

        let mut with_transfers = wire.clone();
        with_transfers["feePayerQuote"]["intent"]["encodedFundTransfers"] = vec!["0x01"].into();
        assert_ne!(recover(with_transfers), signer, "fund transfers added");

        // why: without the tag these two hash the same.
        let mut moved = wire;
        moved["feePayerQuote"] = serde_json::Value::Null;
        moved["multiChainRoot"] = fee_payer_quote.digest().to_string().into();
        assert_ne!(recover(moved), signer, "moved into the root");
    }

    #[test]
    fn fund_transfers_are_refused_on_a_fee_payer_quote() {
        let mut quote = fixed_quote(300);
        assert!(QuoteRole::FeePayer.check(&quote).is_ok(), "no transfers");
        assert!(QuoteRole::User.check(&quote).is_ok());

        quote.intent = quote.intent.with_encoded_fund_transfers(vec![Bytes::from_static(&[1])]);
        assert!(matches!(
            QuoteRole::FeePayer.check(&quote),
            Err(QuoteError::FeePayerFundTransfers)
        ));
        assert!(QuoteRole::User.check(&quote).is_ok(), "a user's output intent still may");
    }

    #[test]
    fn quotes_digest_covers_key_metadata_only_when_present() {
        use crate::types::rpc::{AuthenticatorAttachment, KeyMetadata};

        let quotes = Quotes {
            quotes: vec![],
            ttl: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            multi_chain_root: Some(B256::repeat_byte(3)),
            fee_payer_quote: None,
            key_metadata: vec![],
        };

        // The digest as computed before key metadata existed, so quotes signed by an older
        // relay still verify.
        let mut legacy = Keccak256::new();
        legacy.update(1_700_000_000u64.to_be_bytes());
        legacy.update(B256::repeat_byte(3));
        assert_eq!(quotes.digest(), legacy.finalize());

        let entry = KeyMetadataEntry {
            key_hash: B256::repeat_byte(9),
            metadata: KeyMetadata {
                credential_id: Some("AbC".to_string()),
                attachment: Some(AuthenticatorAttachment::CrossPlatform),
                transports: vec!["usb".to_string()],
            },
        };
        let with = Quotes { key_metadata: vec![entry.clone()], ..quotes.clone() };
        assert_ne!(quotes.digest(), with.digest());

        let mut swapped = entry;
        swapped.metadata.attachment = Some(AuthenticatorAttachment::Platform);
        let swapped = Quotes { key_metadata: vec![swapped], ..quotes };
        assert_ne!(with.digest(), swapped.digest());
    }
}
