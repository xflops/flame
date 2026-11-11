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

use super::*;
mod validation {
    use super::*;
    /// Accept complete object keys with valid application, session, and object components.

    #[test]
    fn object_key_accepts_valid_keys() {
        assert!(ObjectKey::try_from("app/session/object").is_ok());
        assert!(ObjectKey::try_from("my-app/my-session/my-object").is_ok());
        assert!(ObjectKey::try_from("test-app/test-session/test-object").is_ok());
        assert!(ObjectKey::try_from("app1/session1/550e8400-e29b-41d4-a716-446655440000").is_ok());
    }
    /// Reject path traversal in any object key component.

    #[test]
    fn object_key_rejects_path_traversal() {
        assert!(ObjectKey::try_from("../etc/passwd").is_err());
        assert!(ObjectKey::try_from("app/../other/object").is_err());
        assert!(ObjectKey::try_from("app/session/..").is_err());
    }
    /// Require an object component when parsing a complete object key.

    #[test]
    fn object_key_rejects_two_part_keys() {
        assert!(ObjectKey::try_from("session/object").is_err());
    }
    /// Reject object keys containing empty path components.

    #[test]
    fn object_key_rejects_empty_components() {
        assert!(ObjectKey::try_from("app//object").is_err());
        assert!(ObjectKey::try_from("/session/object").is_err());
    }
    /// Parse an application/session path as a session namespace.

    #[test]
    fn object_key_from_path_two_parts() {
        let key = ObjectKey::from_path("my-app/my-session").unwrap();
        assert_eq!(key.app_name, "my-app");
        assert_eq!(key.session_id, "my-session");
        assert!(key.object_id.is_none());
    }
    /// Parse a three-component path as one complete object key.

    #[test]
    fn object_key_from_path_three_parts() {
        let key = ObjectKey::from_path("my-app/my-session/my-uuid").unwrap();
        assert_eq!(key.app_name, "my-app");
        assert_eq!(key.session_id, "my-session");
        assert_eq!(key.object_id, Some("my-uuid".to_string()));
    }
    /// Distinguish a complete object key from a namespace prefix.

    #[test]
    fn object_key_to_key_and_prefix() {
        let key = ObjectKey::from_path("app/session/uuid").unwrap();
        assert_eq!(key.to_key(), Some("app/session/uuid".to_string()));
        assert_eq!(key.to_prefix(), "app/session");
    }
    /// Keep exact object matching from selecting neighboring object names.

    #[test]
    fn object_key_matches_exact_full_key() {
        let full_key = ObjectKey::from_path("app/session/uuid").unwrap();
        let session_key = ObjectKey::from_path("app/session").unwrap();
        let app_key = ObjectKey::from_path("app/*").unwrap();

        assert!(full_key.matches("app/session/uuid"));
        assert!(!full_key.matches("app/session/other"));
        assert!(session_key.matches("app/session/uuid"));
        assert!(session_key.matches("app/session/other"));
        assert!(app_key.matches("app/other/uuid"));
    }
    /// Generate an object ID without changing its application or session.

    #[test]
    fn object_key_with_generated_id() {
        let key = ObjectKey::from_path("app/session").unwrap();
        assert!(key.object_id.is_none());
        let key_with_id = key.with_generated_id();
        assert!(key_with_id.object_id.is_some());
        assert!(key_with_id.to_key().is_some());
    }
}

mod cache_endpoint {
    use super::*;
    /// Preserve the plain gRPC scheme in advertised references.

    #[test]
    fn to_uri_formats_grpc_scheme() {
        let endpoint = CacheEndpoint {
            scheme: "grpc".to_string(),
            host: "localhost".to_string(),
            port: 9090,
        };
        assert_eq!(endpoint.to_uri(), "grpc://localhost:9090");
    }
    /// Convert the internal TLS scheme into the client URI scheme.

    #[test]
    fn to_uri_converts_grpcs_to_grpc_tls() {
        let endpoint = CacheEndpoint {
            scheme: "grpcs".to_string(),
            host: "example.com".to_string(),
            port: 443,
        };
        assert_eq!(endpoint.to_uri(), "grpc+tls://example.com:443");
    }
    /// Bracket IPv6 hosts when formatting advertised references.

    #[test]
    fn to_uri_brackets_ipv6_hosts() {
        let endpoint = CacheEndpoint {
            scheme: "grpc".to_string(),
            host: "2001:db8::1".to_string(),
            port: 9090,
        };
        assert_eq!(endpoint.to_uri(), "grpc://[2001:db8::1]:9090");
    }
    /// Extract the scheme, host, and port from a valid endpoint URL.

    #[test]
    fn try_from_string_parses_valid_endpoint() {
        let endpoint_str = "grpc://localhost:9090".to_string();
        let endpoint = CacheEndpoint::try_from(&endpoint_str).unwrap();
        assert_eq!(endpoint.scheme, "grpc");
        assert_eq!(endpoint.host, "localhost");
        assert_eq!(endpoint.port, 9090);
    }
    /// Default endpoint URLs without a port to 9090.

    #[test]
    fn try_from_string_uses_default_port() {
        let endpoint_str = "grpc://localhost".to_string();
        let endpoint = CacheEndpoint::try_from(&endpoint_str).unwrap();
        assert_eq!(endpoint.port, 9090);
    }
    /// Reject malformed endpoint URLs.

    #[test]
    fn try_from_string_rejects_invalid_url() {
        let endpoint_str = "not a valid url".to_string();
        assert!(CacheEndpoint::try_from(&endpoint_str).is_err());
    }
    /// Reject endpoint URLs that omit a host.

    #[test]
    fn try_from_string_rejects_missing_host() {
        let endpoint_str = "grpc:///path".to_string();
        assert!(CacheEndpoint::try_from(&endpoint_str).is_err());
    }
}

mod advertise {
    use super::*;

    /// Build an endpoint fixture using the supplied advertised host.
    fn endpoint(host: &str) -> CacheEndpoint {
        CacheEndpoint {
            scheme: "grpc".to_string(),
            host: host.to_string(),
            port: 9090,
        }
    }
    /// Return advertised addresses rather than listener addresses in object references.

    #[tokio::test]
    async fn refs_carry_advertised_address() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Arc::new(Controller::new(
            endpoint("cache-1.cache.flame.svc"),
            storage::connect(directory.path().to_str().unwrap())
                .await
                .unwrap(),
        ));

        let key = ObjectKey::from_path("app/session").unwrap();
        let chunks = futures::stream::once(futures::future::ready(Ok(Bytes::from(vec![7u8; 10]))));
        let meta = cache
            .put_stream(WriteRequest {
                key,
                data_type: "raw".to_string(),
                size: 10,
                chunks: Box::pin(chunks),
            })
            .await
            .unwrap();
        assert_eq!(meta.endpoint, "grpc://cache-1.cache.flame.svc:9090");

        let key = ObjectKey::try_from(meta.key.as_str()).unwrap();
        let appended = cache
            .patch(&key, "raw", Bytes::from_static(&[1]))
            .await
            .unwrap();
        assert_eq!(appended.endpoint, "grpc://cache-1.cache.flame.svc:9090");
    }
    /// Check configuration and environment advertisement precedence in isolated subprocesses.

    #[test]
    fn advertised_endpoint_precedence() {
        const CHILD: &str = "FLAME_TEST_ADVERTISE_CHILD";
        if let Ok(expected) = std::env::var(CHILD) {
            let config = FlameCache {
                endpoint: "grpcs://0.0.0.0:9443".into(),
                advertise: std::env::var("FLAME_TEST_CONFIG_ADVERTISE").ok(),
                ..Default::default()
            };
            assert_eq!(advertised_endpoint(&config).unwrap().to_uri(), expected);
            return;
        }

        // Each child owns its environment; parallel tests remain unaffected.
        for (config, env, expected) in [
            (
                None,
                "grpcs://cache-2.cache.flame.svc:9090",
                "grpc+tls://cache-2.cache.flame.svc:9090",
            ),
            (Some(""), "::1", "grpc+tls://[::1]:9443"),
            (
                Some("grpc://from-config:9091"),
                "grpc://from-env:9092",
                "grpc://from-config:9091",
            ),
            (None, "^lo0?$", "grpc+tls://127.0.0.1:9443"),
        ] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "controller::tests::config::advertise::advertised_endpoint_precedence",
                ])
                .env(CHILD, expected)
                .env(ADVERTISE_ENV, env)
                .env_remove("FLAME_TEST_CONFIG_ADVERTISE");
            if let Some(config) = config {
                child.env("FLAME_TEST_CONFIG_ADVERTISE", config);
            }
            let output = child.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{stdout}");
            assert!(
                stdout.contains("1 passed"),
                "child test did not run: {stdout}"
            );
        }
    }
    /// Prefer a local private IPv4 address, falling back to the configured endpoint.

    #[test]
    fn default_advertisement_uses_a_local_private_address_or_endpoint() {
        let config = FlameCache {
            endpoint: "grpc://127.0.0.1:9090".into(),
            ..Default::default()
        };
        let advertised = advertised_endpoint(&config).unwrap();
        let private: Vec<_> = system_interfaces()
            .unwrap()
            .into_values()
            .flatten()
            .filter_map(|ip| match ip {
                IpAddr::V4(ip) if ip.is_private() => Some(ip.to_string()),
                _ => None,
            })
            .collect();
        if private.is_empty() {
            assert_eq!(advertised.to_uri(), config.endpoint);
        } else {
            assert!(private.contains(&advertised.host));
        }
        assert_eq!(advertised.scheme, "grpc");
        assert_eq!(advertised.port, 9090);
        assert_eq!(
            bind_address(&config).unwrap(),
            "0.0.0.0:9090".parse().unwrap()
        );
    }
    /// Resolve interface selectors before interpreting an advertisement as a hostname.

    #[test]
    fn advertise_resolves_interface_first() {
        let config = FlameCache {
            endpoint: "grpcs://0.0.0.0:9443".into(),
            advertise: Some("^lo0?$".into()),
            ..Default::default()
        };
        assert_eq!(
            advertised_endpoint(&config).unwrap().to_uri(),
            "grpc+tls://127.0.0.1:9443"
        );
    }
    /// Preserve supported IP, hostname, and URL advertisement forms.

    #[test]
    fn advertise_literal_address_forms() {
        let config = |advertise: &str| FlameCache {
            endpoint: "grpcs://0.0.0.0:9443".to_string(),
            advertise: Some(advertise.to_string()),
            ..FlameCache::default()
        };
        assert_eq!(
            advertised_endpoint(&config("grpc://pod-0.svc:9000"))
                .unwrap()
                .to_uri(),
            "grpc://pod-0.svc:9000"
        );
        assert_eq!(
            advertised_endpoint(&config("pod-0.svc")).unwrap().to_uri(),
            "grpc+tls://pod-0.svc:9443"
        );
        assert_eq!(
            advertised_endpoint(&config("pod-0.svc:9000"))
                .unwrap()
                .to_uri(),
            "grpc+tls://pod-0.svc:9000"
        );
        assert_eq!(
            advertised_endpoint(&config("192.168.1.5"))
                .unwrap()
                .to_uri(),
            "grpc+tls://192.168.1.5:9443"
        );
        for value in ["::1", "[::1]"] {
            assert_eq!(
                advertised_endpoint(&config(value)).unwrap().to_uri(),
                "grpc+tls://[::1]:9443"
            );
        }
    }
    /// Bind explicit IP selectors using the listener port, including IPv6 addresses.

    #[test]
    fn explicit_ips_bind_the_endpoint_port_including_ipv6() {
        for (value, expected) in [
            ("192.0.2.10", "192.0.2.10:9443"),
            ("::1", "[::1]:9443"),
            ("[::1]", "[::1]:9443"),
        ] {
            let config = FlameCache {
                endpoint: "grpcs://0.0.0.0:9443".into(),
                advertise: Some(value.into()),
                ..Default::default()
            };
            assert_eq!(
                bind_address(&config).unwrap(),
                expected.parse().unwrap(),
                "{value}"
            );
        }
    }
    /// Bind an interface advertisement to the selected local address.

    #[test]
    fn interface_binding_uses_the_selected_ip() {
        let config = FlameCache {
            endpoint: "grpc://0.0.0.0:9090".into(),
            advertise: Some("^lo0?$".into()),
            ..Default::default()
        };
        assert_eq!(
            bind_address(&config).unwrap(),
            "127.0.0.1:9090".parse().unwrap()
        );
    }
    /// Keep wildcard listening for DNS and URL advertisements.

    #[test]
    fn advertised_addresses_that_are_not_ip_or_interface_selectors_bind_all() {
        for value in [
            "cache.example.test",
            "cache.example.test:9000",
            "grpc://lo:9000",
            "grpc-proxy://cache.example.test:9000",
            "grpc://192.0.2.10:9000",
            "192.0.2.10:9000",
            "^flame_nonexistent_interface$",
        ] {
            let config = FlameCache {
                endpoint: "grpc://127.0.0.1:9090".into(),
                advertise: Some(value.into()),
                ..Default::default()
            };
            assert_eq!(
                bind_address(&config).unwrap(),
                "0.0.0.0:9090".parse().unwrap(),
                "{value}"
            );
        }
    }
    /// Treat empty advertisement overrides as absent for references and binding.

    #[test]
    fn empty_advertisements_keep_default_refs_and_wildcard_binding() {
        let mut config = FlameCache {
            endpoint: "grpc://0.0.0.0:9090".into(),
            ..Default::default()
        };
        let default = advertised_endpoint(&config).unwrap().to_uri();
        config.advertise = Some(String::new());
        assert_eq!(advertised_endpoint(&config).unwrap().to_uri(), default);
        assert_eq!(
            bind_address(&config).unwrap(),
            "0.0.0.0:9090".parse().unwrap()
        );
    }
    /// Reject an advertised URL without a valid host.

    #[test]
    fn invalid_advertised_url_is_rejected() {
        let config = FlameCache {
            advertise: Some("grpc://".into()),
            ..Default::default()
        };
        assert!(advertised_endpoint(&config).is_err());
    }
}
