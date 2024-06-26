// SPDX-FileCopyrightText: The Ferrocene Developers
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::keys::newtypes::{PayloadBytes, SignatureBytes};
use crate::keys::{KeyId, KeyPair, KeyRole, PublicKey};
use crate::Error;
use serde::{Deserialize, Serialize};
use std::cell::{Ref, RefCell};

/// Piece of data with signatures attached to it.
///
/// To prevent misuses, there is no way to access the data inside the payload unless signatures are
/// verified. The signed payload can be freely serialized and deserialized.
#[derive(Serialize, Deserialize, Clone)]
#[serde(bound = "T: Signable")]
pub struct SignedPayload<T: Signable> {
    signatures: Vec<Signature>,
    signed: String,
    #[serde(skip)]
    verified_deserialized: RefCell<Option<T>>,
}

impl<T: Signable> std::fmt::Debug for SignedPayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedPayload")
            .field("signatures", &self.signatures)
            .field("signed", &self.signed)
            .finish_non_exhaustive()
    }
}

impl<T: Signable> SignedPayload<T> {
    /// Create a new signed payload. Note that no signature is generated by this method call:
    /// you'll also need to call [`add_signature`](Self::add_signature) with a valid [`KeyPair`] to
    /// generate a valid signed payload.
    pub fn new(to_sign: &T) -> Result<Self, Error> {
        Ok(Self {
            signatures: Vec::new(),
            signed: serde_json::to_string(to_sign)
                .map_err(Error::SignedPayloadSerializationFailed)?,
            verified_deserialized: RefCell::new(None),
        })
    }

    /// Add a new signature to this signed paylaod, generated using the provided [`KeyPair`].
    pub fn add_signature(&mut self, keypair: &dyn KeyPair) -> Result<(), Error> {
        self.signatures.push(Signature {
            key_sha256: keypair.public().calculate_id(),
            signature: keypair.sign(&PayloadBytes::borrowed(self.signed.as_bytes()))?,
        });
        Ok(())
    }

    /// Verifies the signatures attached to the signed payload and returns the deserialized data
    /// (if the signature matched).
    ///
    /// As signature verification and deserialization is expensive, it is only performed the first
    /// time the method is called. The cached results from the initial call will be returned in the
    /// rest of the cases.
    pub fn get_verified(&self, keys: &dyn PublicKeysRepository) -> Result<Ref<'_, T>, Error> {
        let borrow = self.verified_deserialized.borrow();

        if borrow.is_none() {
            let value = verify_signature(
                keys,
                &self.signatures,
                PayloadBytes::borrowed(self.signed.as_bytes()),
            )?;

            // In theory, `borrow_mut()` could panic if an immutable borrow was alive at the same
            // time. In practice that won't happen, as we only populate the cache before returning
            // any reference to the cached data.
            drop(borrow);
            *self.verified_deserialized.borrow_mut() = Some(value);
        }

        Ok(Ref::map(self.verified_deserialized.borrow(), |b| {
            b.as_ref().unwrap()
        }))
    }

    /// Consumes the signed payload and returns the deserialized payload.
    ///
    /// If the signature verification was already performed before (through the
    /// [`get_verified`](Self::get_verified) method), the cached deserialized payload will be
    /// returned. Otherwise, signature verification will be performed with the provided keychain
    /// before deserializing.
    pub fn into_verified(self, keys: &dyn PublicKeysRepository) -> Result<T, Error> {
        if let Some(deserialized) = self.verified_deserialized.into_inner() {
            Ok(deserialized)
        } else {
            verify_signature(
                keys,
                &self.signatures,
                PayloadBytes::borrowed(self.signed.as_bytes()),
            )
        }
    }
}

fn verify_signature<T: Signable>(
    keys: &dyn PublicKeysRepository,
    signatures: &[Signature],
    signed: PayloadBytes<'_>,
) -> Result<T, Error> {
    for signature in signatures {
        let key = match keys.get(&signature.key_sha256) {
            Some(key) => key,
            None => continue,
        };

        match key.verify(T::SIGNED_BY_ROLE, &signed, &signature.signature) {
            Ok(()) => {}
            Err(Error::VerificationFailed) => continue,
            Err(other) => return Err(other),
        }

        // Deserialization is performed after the signature is verified, to ensure we are not
        // deserializing malicious data.
        return serde_json::from_slice(signed.as_bytes()).map_err(Error::DeserializationFailed);
    }

    Err(Error::VerificationFailed)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Signature {
    key_sha256: KeyId,
    #[serde(with = "crate::serde_base64")]
    signature: SignatureBytes<'static>,
}

/// Trait representing contents that can be wrapped in a [`SignedPayload`].
pub trait Signable: Serialize + for<'de> Deserialize<'de> {
    /// Key role authorized to verify this type.
    const SIGNED_BY_ROLE: KeyRole;
}

/// Trait representing a collection of public keys that can be used to verify signatures.
///
/// You likely want to use a [`Keychain`](crate::signatures::Keychain) as the public keys
/// repository, as it allows to establish a root of trust and supports multiple keys. For simple
/// cases or tests, individual [`PublicKey`]s also implement this trait.
pub trait PublicKeysRepository {
    /// Retrieve a key by its ID.
    fn get<'a>(&'a self, id: &KeyId) -> Option<&'a PublicKey>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{EphemeralKeyPair, PublicKey};
    use crate::signatures::Keychain;
    use crate::test_utils::{base64_encode, TestEnvironment};

    const SAMPLE_DATA: &str = r#"{"answer":42}"#;

    #[test]
    fn tets_verify_no_signatures() {
        let test_env = TestEnvironment::prepare();
        assert_verify_fail(&test_env, &[]);
    }

    #[test]
    fn test_verify_one_valid_signature() {
        let mut test_env = TestEnvironment::prepare();

        let key = test_env.create_key(KeyRole::Packages);
        assert_verify_pass(&test_env, &[&key]);
    }

    #[test]
    fn test_verify_multiple_valid_signatures() {
        let mut test_env = TestEnvironment::prepare();

        let key1 = test_env.create_key(KeyRole::Packages);
        let key2 = test_env.create_key(KeyRole::Packages);

        assert_verify_pass(&test_env, &[&key1, &key2]);
        assert_verify_pass(&test_env, &[&key2, &key1]);
    }

    // Key roles

    #[test]
    fn test_verify_with_invalid_key_role() {
        let mut test_env = TestEnvironment::prepare();

        let key = test_env.create_key(KeyRole::Redirects);
        assert_verify_fail(&test_env, &[&key]);
    }

    #[test]
    fn test_verify_with_invalid_and_valid_key_roles() {
        let mut test_env = TestEnvironment::prepare();

        let valid = test_env.create_key(KeyRole::Packages);
        let invalid = test_env.create_key(KeyRole::Redirects);
        assert_verify_pass(&test_env, &[&valid, &invalid]);
        assert_verify_pass(&test_env, &[&invalid, &valid]);
    }

    // Trusted/untrusted
    #[test]
    fn test_verify_with_untrusted_key() {
        let test_env = TestEnvironment::prepare();

        let untrusted = test_env.create_untrusted_key(KeyRole::Packages);
        assert_verify_fail(&test_env, &[&untrusted]);
    }

    #[test]
    fn test_verify_with_trusted_and_untrusted_keys() {
        let mut test_env = TestEnvironment::prepare();

        let trusted = test_env.create_key(KeyRole::Packages);
        let untrusted = test_env.create_untrusted_key(KeyRole::Packages);

        assert_verify_pass(&test_env, &[&trusted, &untrusted]);
        assert_verify_pass(&test_env, &[&untrusted, &trusted]);
    }

    #[test]
    fn test_verify_with_subset_of_trusted_keys() {
        let mut test_env = TestEnvironment::prepare();

        let used_key = test_env.create_key(KeyRole::Packages);
        let _other_trusted_key = test_env.create_key(KeyRole::Packages);

        assert_verify_pass(&test_env, &[&used_key]);
    }

    // Expiry

    #[test]
    fn test_verify_with_expired_key() {
        let mut test_env = TestEnvironment::prepare();

        let expired = test_env.create_key_with_expiry(KeyRole::Packages, -1);
        assert_verify_fail(&test_env, &[&expired]);
    }

    #[test]
    fn test_verify_with_not_expired_key() {
        let mut env = TestEnvironment::prepare();

        let not_expired = env.create_key_with_expiry(KeyRole::Packages, 1);
        assert_verify_pass(&env, &[&not_expired]);
    }

    #[test]
    fn test_verify_with_expired_and_not_expired_keys() {
        let mut test_env = TestEnvironment::prepare();

        let expired = test_env.create_key_with_expiry(KeyRole::Packages, -1);
        let not_expired = test_env.create_key_with_expiry(KeyRole::Packages, 1);

        assert_verify_pass(&test_env, &[&expired, &not_expired]);
        assert_verify_pass(&test_env, &[&not_expired, &expired]);
    }

    // Signature

    #[test]
    fn test_verify_with_bad_signature() {
        let mut test_env = TestEnvironment::prepare();

        let bad = BadKeyPair(test_env.create_key(KeyRole::Packages));
        assert_verify_fail(&test_env, &[&bad]);
    }

    #[test]
    fn test_verify_with_bad_and_good_signature() {
        let mut test_env = TestEnvironment::prepare();

        let bad = BadKeyPair(test_env.create_key(KeyRole::Packages));
        let good = test_env.create_key(KeyRole::Packages);
        assert_verify_pass(&test_env, &[&bad, &good]);
        assert_verify_pass(&test_env, &[&good, &bad]);
    }

    // Caching

    #[test]
    fn test_caching() {
        let mut test_env = TestEnvironment::prepare();

        let key = test_env.create_key(KeyRole::Packages);
        let payload = prepare_payload(&[&key], SAMPLE_DATA);

        assert_eq!(
            42,
            payload.get_verified(test_env.keychain()).unwrap().answer
        );

        // If there was no caching, this method call would fail, as there is no valid key to
        // perform verification in an empty keychain. Still, since there is a cache no signature
        // verification is performed and the previous result is returned.
        assert_eq!(
            42,
            payload
                .get_verified(TestEnvironment::prepare().keychain())
                .unwrap()
                .answer
        );
    }

    // Misc tests

    #[test]
    fn test_deserialization_failed() {
        let mut test_env = TestEnvironment::prepare();
        let key = test_env.create_key(KeyRole::Packages);

        let payload = prepare_payload(&[&key], r#"{"answer": 42"#);
        assert!(matches!(
            payload.get_verified(test_env.keychain()),
            Err(Error::DeserializationFailed(_))
        ));

        let payload = prepare_payload(&[&key], r#"{"answer": 42"#);
        assert!(matches!(
            payload.into_verified(test_env.keychain()),
            Err(Error::DeserializationFailed(_))
        ));
    }

    #[test]
    fn test_verify_deserialized() {
        let mut keychain = Keychain::new(
            &serde_json::from_str(
                r#"{
                    "role": "root",
                    "algorithm": "ecdsa-p256-sha256-asn1-spki-der",
                    "expiry": null,
                    "public": "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE+S7QgNLkBo2VEMdZXowZUFmvQJMm6qoQtC33hvDB95HpjPXd50eBEUnEuVRye5qC84K7ZHpoAXWf5BzmcFtvVg=="
                }"#,
            )
            .unwrap(),
        ).unwrap();

        keychain.load(
            &serde_json::from_str(
                r#"{
                    "signatures": [
                        {
                            "key_sha256": "oWLXbXl20A0Z5MNOcEC4vNjHxT3IHAo9ExDYMAyHatU=",
                            "signature": "MEUCIQDY3xkoVYowUQBSnHddpWVdlG9FufeucTasX9YJNOzPsQIgRj99gqJioVB6TLa9gdmPezFG68CC+tAkqGA9GwfVurs="
                        }
                    ],
                    "signed": "{\"role\":\"packages\",\"algorithm\":\"ecdsa-p256-sha256-asn1-spki-der\",\"expiry\":null,\"public\":\"MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAExmWCqNu5ClVwVgoMYU/cRUTTohljVT5yJy5InJPzXaXRQS7zT5WaTUxzJQqfDc7+nUgEZ6Z6XbxzG72yffrckA==\"}"
                }"#,
            )
            .unwrap(),
        ).unwrap();

        let payload: SignedPayload<TestData> = serde_json::from_str(
            r#"{
                "signatures": [
                    {
                        "key_sha256": "xzcGUBKHYDGbucyvirl6dhsDXPCxQR/4/PRKiL9Qz2A=",
                        "signature": "MEYCIQCToeOQpzoZxYSBaBcb1Ko+NFtr4/fmLwaTrrvuWagzQgIhAO8AvDZHk+osFj0Wag5MU9CzQeXgCi4Cr8FCk4KhKVX6"
                    }
                ],
                "signed": "{\"answer\":42}"
            }"#,
        ).unwrap();

        assert_eq!(42, payload.get_verified(&keychain).unwrap().answer);
    }

    // Utilities

    #[track_caller]
    fn assert_verify_pass(test_env: &TestEnvironment, keys: &[&dyn KeyPair]) {
        let get_payload = prepare_payload(keys, SAMPLE_DATA);
        assert_eq!(
            42,
            get_payload
                .get_verified(test_env.keychain())
                .unwrap()
                .answer
        );

        // Two separate payloads are used to avoid caching.
        let into_payload = prepare_payload(keys, SAMPLE_DATA);
        assert_eq!(
            42,
            into_payload
                .into_verified(test_env.keychain())
                .unwrap()
                .answer
        );
    }

    #[track_caller]
    fn assert_verify_fail(test_env: &TestEnvironment, keys: &[&dyn KeyPair]) {
        let get_payload = prepare_payload(keys, SAMPLE_DATA);
        assert!(matches!(
            get_payload.get_verified(test_env.keychain()).unwrap_err(),
            Error::VerificationFailed
        ));

        // Two separate payloads are used to avoid caching.
        let into_payload = prepare_payload(keys, SAMPLE_DATA);
        assert!(matches!(
            into_payload.into_verified(test_env.keychain()).unwrap_err(),
            Error::VerificationFailed
        ));
    }

    fn prepare_payload(keys: &[&dyn KeyPair], data: &str) -> SignedPayload<TestData> {
        serde_json::from_value(serde_json::json!({
            "signatures": keys
                .iter()
                .map(|key| {
                    serde_json::json!({
                        "key_sha256": key.public().calculate_id(),
                        "signature": base64_encode(key.sign(
                            &PayloadBytes::borrowed(data.as_bytes())
                        ).unwrap().as_bytes()),
                    })
                })
                .collect::<Vec<_>>(),
            "signed": data
        }))
        .unwrap()
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct TestData {
        answer: i32,
    }

    impl Signable for TestData {
        const SIGNED_BY_ROLE: KeyRole = KeyRole::Packages;
    }

    struct BadKeyPair(EphemeralKeyPair);

    impl KeyPair for BadKeyPair {
        fn public(&self) -> &PublicKey {
            self.0.public()
        }

        fn sign(&self, data: &PayloadBytes<'_>) -> Result<SignatureBytes<'static>, Error> {
            let signature = self.0.sign(data)?;
            let mut broken_signature = signature.as_bytes().to_vec();
            for byte in &mut broken_signature {
                *byte = byte.wrapping_add(1);
            }

            Ok(SignatureBytes::owned(broken_signature))
        }
    }
}
