/*
Copyright 2023 The Flame Authors.
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
use std::net::IpAddr;

mod frontend;

#[test]
fn bind_defaults_and_literal_ips_do_not_discover_interfaces() {
    for (value, expected) in [
        (None, "0.0.0.0"),
        (Some(""), "0.0.0.0"),
        (Some("127.0.0.1"), "127.0.0.1"),
        (Some("192.0.2.10"), "192.0.2.10"),
        (Some("::1"), "::1"),
    ] {
        let mut ctx = FlameClusterContext::default();
        ctx.cluster.advertise = value.map(str::to_string);
        assert_eq!(
            bind_address(&ctx, 0).unwrap().ip(),
            expected.parse::<IpAddr>().unwrap()
        );
    }
}

#[test]
fn bind_invalid_interfaces_fail_without_wildcard_fallback() {
    for value in ["[", "^flame_nonexistent_interface$", "grpc://cache:9090"] {
        let mut ctx = FlameClusterContext::default();
        ctx.cluster.advertise = Some(value.into());
        assert!(bind_address(&ctx, 0).is_err());
    }
    let mut ctx = FlameClusterContext::default();
    ctx.cluster.advertise = Some("^lo0?$".into());
    assert_eq!(
        bind_address(&ctx, 0).unwrap().ip(),
        "127.0.0.1".parse::<IpAddr>().unwrap()
    );
}

#[test]
fn both_listener_addresses_use_advertise_and_endpoint_ports() {
    let mut ctx = FlameClusterContext::default();
    ctx.cluster.endpoint = "http://service.flame:18080".into();
    ctx.cluster.advertise = Some("127.0.0.1".into());
    assert_eq!(
        bind_address(&ctx, 0).unwrap(),
        "127.0.0.1:18080".parse().unwrap()
    );
    assert_eq!(
        bind_address(&ctx, 1).unwrap(),
        "127.0.0.1:18081".parse().unwrap()
    );
    ctx.cluster.advertise = Some("::1".into());
    ctx.cluster.endpoint = "http://service.flame".into();
    assert_eq!(
        bind_address(&ctx, 1).unwrap(),
        "[::1]:8081".parse().unwrap()
    );
    ctx.cluster.endpoint = "http://service.flame:65535".into();
    assert!(bind_address(&ctx, 1).is_err());
    ctx.cluster.endpoint = "invalid endpoint".into();
    assert!(bind_address(&ctx, 0).is_err());
}
