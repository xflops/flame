/*
Copyright 2025 The Flame Authors.
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

/// Select `ring` as the process-wide rustls crypto provider.
///
/// Dependencies in the same build can enable both the `ring` and `aws-lc-rs` features of
/// rustls; rustls then refuses to pick a default and panics on the first TLS handshake.
/// Installing a provider explicitly keeps the choice independent of feature unification.
/// Calling this more than once is harmless: the first provider installed wins.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
