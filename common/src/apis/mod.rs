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

mod executor;
mod filters;
mod from_rpc;
mod node_info;
mod pagination;
mod session;
mod to_rpc;
mod types;

pub use executor::{Executor, ExecutorPtr};
pub use filters::*;
pub use pagination::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resreq_from_string() {
        let cases = vec![
            ("cpu=1,mem=256", (1, 256)),
            ("cpu=1,mem=1k", (1, 1024)),
            ("cpu=1,memory=1m", (1, 1024 * 1024)),
            ("cpu=1,memory=1g", (1, 1024 * 1024 * 1024)),
        ];

        for (input, expected) in cases {
            let resreq = ResourceRequirement::from(input);
            assert_eq!(resreq.cpu, expected.0);
            assert_eq!(resreq.memory, expected.1);
        }
    }

    #[test]
    fn test_resreq_parse_rejects_malformed_input() {
        for input in ["cpu=abc", "mem=bogus", "gpu=", "foo=1", "cpu=1,"] {
            assert!(
                ResourceRequirement::parse(input).is_err(),
                "expected invalid resreq to fail: {input}"
            );
        }
    }

    #[test]
    fn test_node_shims_rpc_round_trip() {
        use rpc::flame::v1 as rpc;
        let node = Node {
            name: "node-1".to_string(),
            shims: vec![Shim::Cri, Shim::Host],
            ..Default::default()
        };

        let wire = rpc::Node::from(&node);
        assert_eq!(
            wire.spec.as_ref().unwrap().shims,
            [rpc::Shim::Cri as i32, rpc::Shim::Host as i32]
        );
        assert_eq!(Node::from(wire).shims, [Shim::Cri, Shim::Host]);

        // A node that reports none, and a value this version does not know.
        assert!(Node::from(rpc::Node::from(&Node::default()))
            .shims
            .is_empty());
        let mut wire = rpc::Node::from(&node);
        wire.spec.as_mut().unwrap().shims.push(99);
        assert_eq!(Node::from(wire).shims, [Shim::Cri, Shim::Host]);
    }

    #[test]
    fn node_inventory_round_trips_and_reports_assigned_addresses() {
        let node = Node {
            name: "worker".into(),
            info: NodeInfo {
                arch: "x86_64".into(),
                os: "linux".into(),
                kernel_version: "6.8.0".into(),
                os_image: "Ubuntu".into(),
                network_interfaces: vec![NodeNetworkInterface {
                    name: "eth0".into(),
                    index: 2,
                    mac_address: "02:00:00:00:00:01".into(),
                    addresses: vec![
                        "192.168.3.149/24".into(),
                        "100.117.27.41/32".into(),
                        "192.168.3.149/24".into(),
                        "127.0.0.1/8".into(),
                        "::1/128".into(),
                        "fe80::1/64".into(),
                        "bad".into(),
                    ],
                    mtu: 1500,
                    state: "up".into(),
                    pci_address: "0000:01:00.0".into(),
                }],
                pci_devices: vec![NodePciDevice {
                    address: "0000:01:00.0".into(),
                    vendor_id: "8086".into(),
                    device_id: "1234".into(),
                    class_id: "020000".into(),
                    driver: "igc".into(),
                    numa_node: Some(0),
                }],
            },
            ..Default::default()
        };
        let wire = rpc::flame::v1::Node::from(&node);
        let addresses = &wire.status.as_ref().unwrap().addresses;
        assert_eq!(
            addresses
                .iter()
                .map(|a| (a.r#type.as_str(), a.address.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Hostname", "worker"),
                ("InternalIP", "100.117.27.41"),
                ("InternalIP", "192.168.3.149")
            ]
        );
        assert_eq!(Node::from(wire).info, node.info);
        assert!(NodeInfo::from(rpc::flame::v1::NodeInfo::default())
            .network_interfaces
            .is_empty());
    }

    #[test]
    fn test_session_state_rpc_round_trip() {
        use rpc::flame::v1 as rpc;
        for (state, wire) in [
            (SessionState::Open, 0),
            (SessionState::Closed, 1),
            (SessionState::Suspending, 2),
            (SessionState::Suspended, 3),
        ] {
            assert_eq!(i32::from(state), wire);
            assert_eq!(rpc::SessionState::from(state) as i32, wire);
            assert_eq!(SessionState::try_from(wire).unwrap(), state);
        }
        assert!(SessionState::try_from(99).is_err());
    }

    #[test]
    fn test_shim_default() {
        let shim = Shim::default();
        assert_eq!(shim, Shim::Host);
    }

    #[test]
    fn test_shim_from_string() {
        assert_eq!(Shim::try_from("host".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("Host".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("HOST".to_string()).unwrap(), Shim::Host);
        assert_eq!(Shim::try_from("wasm".to_string()).unwrap(), Shim::Wasm);
        assert_eq!(Shim::try_from("Wasm".to_string()).unwrap(), Shim::Wasm);
        assert_eq!(Shim::try_from("WASM".to_string()).unwrap(), Shim::Wasm);
        assert_eq!(Shim::try_from("cri".to_string()).unwrap(), Shim::Cri);
        assert_eq!(Shim::try_from("CRI".to_string()).unwrap(), Shim::Cri);
        assert!(Shim::try_from("invalid".to_string()).is_err());
    }

    #[test]
    fn test_application_attributes_default_shim() {
        let attrs = ApplicationAttributes::default();
        assert_eq!(attrs.shim, Shim::Host);
    }

    mod validate_application_name {
        use super::*;

        #[test]
        fn valid_names() {
            assert!(validate_application_name("my-app").is_ok());
            assert!(validate_application_name("my_app").is_ok());
            assert!(validate_application_name("my.app").is_ok());
            assert!(validate_application_name("myapp123").is_ok());
            assert!(validate_application_name("MyApp").is_ok());
            assert!(validate_application_name("a").is_ok());
        }

        #[test]
        fn empty_name() {
            assert!(validate_application_name("").is_err());
        }

        #[test]
        fn path_traversal() {
            assert!(validate_application_name("..").is_err());
            assert!(validate_application_name("../etc").is_err());
            assert!(validate_application_name("app/../../etc").is_err());
            assert!(validate_application_name("app\\..\\etc").is_err());
        }

        #[test]
        fn starts_with_dot_or_dash() {
            assert!(validate_application_name(".hidden").is_err());
            assert!(validate_application_name("-invalid").is_err());
        }

        #[test]
        fn invalid_characters() {
            assert!(validate_application_name("app name").is_err());
            assert!(validate_application_name("app@name").is_err());
            assert!(validate_application_name("app#name").is_err());
            assert!(validate_application_name("app$name").is_err());
        }

        #[test]
        fn too_long() {
            let long_name = "a".repeat(254);
            assert!(validate_application_name(&long_name).is_err());
            let ok_name = "a".repeat(253);
            assert!(validate_application_name(&ok_name).is_ok());
        }
    }
}
