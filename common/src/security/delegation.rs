/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::fs;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use openssl::error::ErrorStack;
use openssl::md::Md;
use openssl::pkey::{Id, PKey, Private, Public};
use openssl::pkey_ctx::PkeyCtx;
use openssl::sign::{Signer, Verifier};
use openssl::x509::X509;

use crate::ctx::FlameTls;
use crate::FlameError;

pub struct DelegationManager {
    private_key: PKey<Private>,
    public_key: PKey<Public>,
}

const SIGNATURE_LENGTH: usize = 64;
const TOKEN_LENGTH: usize = 86;
impl DelegationManager {
    pub fn from_tls(tls: &FlameTls, domain: &str) -> Result<Self, FlameError> {
        let certificate = X509::from_pem(&fs::read(&tls.cert_file)?).map_err(|error| {
            FlameError::InvalidConfig(format!("invalid delegation certificate: {error}"))
        })?;
        let private_key =
            PKey::private_key_from_pem(&fs::read(&tls.key_file)?).map_err(|error| {
                FlameError::InvalidConfig(format!("invalid delegation private key: {error}"))
            })?;
        let certificate_key = certificate.public_key().map_err(|error| {
            FlameError::InvalidConfig(format!("invalid delegation certificate key: {error}"))
        })?;
        if !certificate_key.public_eq(&private_key) {
            return Err(FlameError::InvalidConfig(
                "delegation certificate and private key do not match".to_string(),
            ));
        }
        let encoded_key = private_key.private_key_to_pkcs8().map_err(|error| {
            FlameError::InvalidConfig(format!("invalid delegation private key: {error}"))
        })?;
        let seed = derive_seed(&encoded_key, domain).map_err(|error| {
            FlameError::InvalidConfig(format!("delegation key derivation failed: {error}"))
        })?;
        Self::from_seed(&seed)
    }

    fn from_seed(seed: &[u8; 32]) -> Result<Self, FlameError> {
        let private_key = PKey::private_key_from_raw_bytes(seed, Id::ED25519).map_err(|error| {
            FlameError::InvalidConfig(format!("delegation key creation failed: {error}"))
        })?;
        let public_key = PKey::public_key_from_raw_bytes(
            &private_key.raw_public_key().map_err(|error| {
                FlameError::InvalidConfig(format!("delegation public key failed: {error}"))
            })?,
            Id::ED25519,
        )
        .map_err(|error| {
            FlameError::InvalidConfig(format!("delegation public key failed: {error}"))
        })?;
        Ok(Self {
            private_key,
            public_key,
        })
    }

    pub fn sign(&self, key: &str) -> Result<String, FlameError> {
        let mut signer = Signer::new_without_digest(&self.private_key)
            .map_err(|error| FlameError::Internal(format!("delegation signer error: {error}")))?;
        let signature = signer
            .sign_oneshot_to_vec(key.as_bytes())
            .map_err(|error| FlameError::Internal(format!("delegation signer error: {error}")))?;
        Ok(URL_SAFE_NO_PAD.encode(signature))
    }

    pub fn verify(&self, token: &str, key: &str) -> bool {
        if token.len() != TOKEN_LENGTH {
            return false;
        }
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(token) else {
            return false;
        };
        let Ok(signature): Result<[u8; SIGNATURE_LENGTH], _> = decoded.try_into() else {
            return false;
        };
        let Ok(mut verifier) = Verifier::new_without_digest(&self.public_key) else {
            return false;
        };
        verifier
            .verify_oneshot(&signature, key.as_bytes())
            .unwrap_or(false)
    }
}

fn derive_seed(encoded_key: &[u8], domain: &str) -> Result<[u8; 32], ErrorStack> {
    let mut derivation = PkeyCtx::new_id(Id::HKDF)?;
    derivation.derive_init()?;
    derivation.set_hkdf_md(Md::sha256())?;
    derivation.set_hkdf_key(encoded_key)?;
    derivation.set_hkdf_salt(domain.as_bytes())?;
    let mut seed = [0; 32];
    derivation.derive(Some(&mut seed))?;
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::rsa::Rsa;
    use openssl::x509::X509Name;
    use tempfile::TempDir;

    fn test_manager() -> (TempDir, DelegationManager) {
        let dir = tempfile::tempdir().unwrap();
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509Name::builder().unwrap();
        name.append_entry_by_text("CN", "cache-test").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        let serial = BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap();
        cert.set_serial_number(&serial).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(Asn1Time::days_from_now(0).unwrap().as_ref())
            .unwrap();
        cert.set_not_after(Asn1Time::days_from_now(1).unwrap().as_ref())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let cert_file = dir.path().join("cache.crt");
        let key_file = dir.path().join("cache.key");
        fs::write(&cert_file, cert.build().to_pem().unwrap()).unwrap();
        fs::write(&key_file, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        let config = FlameTls {
            cert_file: cert_file.to_string_lossy().into_owned(),
            key_file: key_file.to_string_lossy().into_owned(),
            ca_file: None,
        };
        let manager = DelegationManager::from_tls(&config, "cache-signing-v1").unwrap();
        (dir, manager)
    }

    #[test]
    fn token_is_bound_to_the_full_object_key() {
        let (_dir, manager) = test_manager();
        let key = "alice/app/ssn/object";
        let token = manager.sign(key).unwrap();
        assert_eq!(token.len(), TOKEN_LENGTH);
        assert!(!token.contains('/'));
        assert!(manager.verify(&token, key));
        assert!(!manager.verify(&token, "alice/app/ssn/other"));
        assert!(!manager.verify(&token, "bob/app/ssn/object"));
        assert!(!manager.verify(&format!("{token}x"), key));
    }

    #[test]
    fn another_replica_with_same_certificate_verifies_tokens() {
        let (dir, manager) = test_manager();
        let config = FlameTls {
            cert_file: dir.path().join("cache.crt").to_string_lossy().into_owned(),
            key_file: dir.path().join("cache.key").to_string_lossy().into_owned(),
            ca_file: None,
        };
        let another_replica = DelegationManager::from_tls(&config, "cache-signing-v1").unwrap();
        let another_domain = DelegationManager::from_tls(&config, "other-signing-v1").unwrap();
        let (_other_dir, other_certificate) = test_manager();
        let key = "alice/app/ssn/object";
        let token = manager.sign(key).unwrap();
        assert!(another_replica.verify(&token, key));
        assert!(!another_domain.verify(&token, key));
        assert!(!other_certificate.verify(&token, key));
    }

    #[test]
    fn cache_signer_uses_tls_key_and_rotation_invalidates_tokens() {
        let (first_dir, _) = test_manager();
        let first = FlameTls {
            cert_file: first_dir
                .path()
                .join("cache.crt")
                .to_string_lossy()
                .into_owned(),
            key_file: first_dir
                .path()
                .join("cache.key")
                .to_string_lossy()
                .into_owned(),
            ca_file: None,
        };
        let token = DelegationManager::from_tls(&first, "cache-signing-v1")
            .unwrap()
            .sign("app:app")
            .unwrap();
        assert!(DelegationManager::from_tls(&first, "cache-signing-v1")
            .unwrap()
            .verify(&token, "app:app"));

        let (rotated_dir, _) = test_manager();
        let rotated = FlameTls {
            cert_file: rotated_dir
                .path()
                .join("cache.crt")
                .to_string_lossy()
                .into_owned(),
            key_file: rotated_dir
                .path()
                .join("cache.key")
                .to_string_lossy()
                .into_owned(),
            ca_file: None,
        };
        assert!(!DelegationManager::from_tls(&rotated, "cache-signing-v1")
            .unwrap()
            .verify(&token, "app:app"));
    }
}
