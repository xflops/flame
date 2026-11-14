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

use super::*;
use flame_rs::client::{Node, NodeAddress, NodeNetworkInterface, NodePciDevice, NodeState};

/// Render IPv4/IPv6 interface addresses and their corresponding PCI device.
#[test]
fn node_details_show_network_and_pci_inventory() {
    let node = Node {
        name: "node-1".into(),
        hostname: "host-1".into(),
        state: NodeState::Ready,
        cpu: 16,
        memory: 8 * 1024_u64.pow(3),
        arch: "x86_64".into(),
        os: "linux".into(),
        kernel_version: "6.8.0".into(),
        os_image: "Ubuntu 24.04 LTS".into(),
        addresses: vec![
            NodeAddress {
                address_type: "Hostname".into(),
                address: "host-1".into(),
            },
            NodeAddress {
                address_type: "InternalIP".into(),
                address: "192.0.2.10".into(),
            },
            NodeAddress {
                address_type: "InternalIP".into(),
                address: "2001:db8::10".into(),
            },
        ],
        network_interfaces: vec![NodeNetworkInterface {
            name: "eth0".into(),
            index: 2,
            mac_address: "02:00:00:00:00:01".into(),
            addresses: vec!["192.0.2.10/24".into(), "2001:db8::10/64".into()],
            mtu: 1500,
            state: "up".into(),
            pci_address: "0000:01:00.0".into(),
        }],
        pci_devices: vec![NodePciDevice {
            address: "0000:01:00.0".into(),
            vendor_id: "8086".into(),
            device_id: "100e".into(),
            class_id: "020000".into(),
            driver: "e1000".into(),
            numa_node: Some(0),
        }],
        ..Default::default()
    };
    let output = format_node(&node);
    for detail in [
        "node-1",
        "host-1",
        "Ready",
        "Ubuntu 24.04 LTS",
        "6.8.0",
        "Addresses:",
        "192.0.2.10",
        "eth0",
        "PCI Devices:",
        "0000:01:00.0",
        "8086",
        "100e",
        "020000",
        "e1000",
    ] {
        assert!(output.contains(detail), "missing {detail}: {output}");
    }
    let address_rows = output
        .split("Addresses:\n")
        .nth(1)
        .unwrap()
        .split("PCI Devices:")
        .next()
        .unwrap();
    assert_eq!(
        address_rows
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [vec!["eth0", "192.0.2.10"], vec!["eth0", "2001:db8::10"]]
    );
    assert!(!output.contains("Interfaces:"));
    assert!(!output.contains("ADDRESS"));
    assert!(!output.contains('┌'));
    assert!(!output.contains("None"));
    let device_line = output.lines().find(|line| line.contains("e1000")).unwrap();
    assert_eq!(device_line.split_whitespace().last(), Some("0"));
}

/// Distinguish unavailable discovery from fabricated interface/device details.
#[test]
fn node_details_show_missing_inventory_as_dashes() {
    let node = Node {
        name: "node-1".into(),
        ..Default::default()
    };
    let output = format_node(&node);
    assert!(output.starts_with("Name:          node-1\n"));
    assert!(output.contains("Addresses:\n  -\n"));
    assert!(output.contains("PCI Devices:\n  -"));
    assert!(output.contains("  OS Image:    -\n"));
    assert!(output.contains("  Kernel:      -\n"));
    assert!(!output.contains('┌'));
}

/// Virtual interfaces and devices with unavailable properties retain placeholders.
#[test]
fn partial_devices_and_virtual_interfaces_remain_readable() {
    let node = Node {
        addresses: vec![NodeAddress {
            address_type: "InternalIP".into(),
            address: "192.0.2.1".into(),
        }],
        network_interfaces: vec![NodeNetworkInterface {
            name: "virtual0".into(),
            ..Default::default()
        }],
        pci_devices: vec![NodePciDevice {
            address: "0000:01:00.0".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let output = format_node(&node);
    let address_row = output
        .lines()
        .find(|line| line.contains("192.0.2.1"))
        .unwrap();
    assert_eq!(
        address_row.split_whitespace().collect::<Vec<_>>(),
        ["-", "192.0.2.1"]
    );
    assert!(output.contains("0000:01:00.0"));
    assert!(!output.contains("None"));
    assert!(!output.contains("Some("));
    let device_line = output
        .lines()
        .find(|line| line.contains("0000:01:00.0"))
        .unwrap();
    assert_eq!(
        device_line.split_whitespace().collect::<Vec<_>>(),
        ["0000:01:00.0", "-", "-", "-", "-", "-"]
    );
}
