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

/// Builds native interface input with duplicate addresses for normalization tests.
fn interface(name: &str, index: u32) -> NetworkInterface {
    let mut interface = NetworkInterface::new_afinet(
        name,
        "192.0.2.10".parse().unwrap(),
        Some("255.255.255.0".parse().unwrap()),
        None,
        index,
        false,
    )
    .with_mac_addr(Some("AA:BB:CC:DD:EE:FF".into()));
    interface.addr.extend(interface.addr.clone());
    interface
}

/// Creates one valid PCI fixture with optional attributes added by each test.
fn device(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join("bus/pci/devices").join(name);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("vendor"), "0x8086\n").unwrap();
    fs::write(path.join("device"), "0X1234\n").unwrap();
    fs::write(path.join("class"), "0x020000\n").unwrap();
    path
}

#[test]
fn interface_inventory_is_sorted_and_deduplicates_addresses() {
    let interfaces = network_interfaces(
        vec![
            interface("eth1", 2),
            interface("eth0", 1),
            interface("eth0", 1),
        ],
        None,
    );
    assert_eq!(interfaces.len(), 2);
    assert_eq!(interfaces[0].name, "eth0");
    assert_eq!(interfaces[0].index, 1);
    assert_eq!(interfaces[0].addresses, ["192.0.2.10/24"]);
    assert_eq!(interfaces[0].mac_address, "aa:bb:cc:dd:ee:ff");
    assert_eq!(interfaces[0].mtu, 0);
    assert!(interfaces[0].state.is_empty());
    assert!(interfaces[0].pci_address.is_empty());
    assert_eq!(interfaces[1].name, "eth1");
}

#[test]
fn addresses_preserve_ipv6_without_inventing_invalid_prefixes() {
    let v6 = NetworkInterface::new_afinet6(
        "eth0",
        "2001:db8::5".parse().unwrap(),
        Some("ffff:ffff:ffff:ffff::".parse().unwrap()),
        None,
        1,
        false,
    );
    assert_eq!(address(v6.addr[0]), "2001:db8::5/64");
    let mut v4 = interface("eth0", 1);
    if let Addr::V4(addr) = &mut v4.addr[0] {
        addr.netmask = Some("255.0.255.0".parse().unwrap());
    }
    assert_eq!(address(v4.addr[0]), "192.0.2.10");
    if let Addr::V4(addr) = &mut v4.addr[0] {
        addr.netmask = None;
    }
    assert_eq!(address(v4.addr[0]), "192.0.2.10");
}

#[test]
fn linux_interface_attributes_are_optional_and_override_native_mac() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("class/net/eth0");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("address"), "12:34:56:78:9A:BC\n").unwrap();
    fs::write(path.join("mtu"), "9000\n").unwrap();
    fs::write(path.join("operstate"), "up\n").unwrap();
    let interfaces = network_interfaces(vec![interface("eth0", 1)], Some(temp.path()));
    assert_eq!(interfaces[0].mac_address, "12:34:56:78:9a:bc");
    assert_eq!(interfaces[0].mtu, 9000);
    assert_eq!(interfaces[0].state, "up");
    assert!(interfaces[0].pci_address.is_empty());
    fs::write(path.join("mtu"), "invalid").unwrap();
    let interfaces = network_interfaces(vec![interface("eth0", 1)], Some(temp.path()));
    assert_eq!(interfaces[0].mtu, 0);
    assert_eq!(interfaces[0].addresses, ["192.0.2.10/24"]);
}

#[test]
#[cfg(unix)]
fn network_pci_association_rejects_usb_paths() {
    let temp = tempfile::tempdir().unwrap();
    let nic = temp.path().join("class/net/eth0");
    fs::create_dir_all(&nic).unwrap();
    let pci = device(temp.path(), "0000:03:00.0");
    std::os::unix::fs::symlink(&pci, nic.join("device")).unwrap();
    assert_eq!(
        network_interfaces(vec![interface("eth0", 1)], Some(temp.path()))[0].pci_address,
        "0000:03:00.0"
    );
    fs::remove_file(nic.join("device")).unwrap();
    let usb = temp.path().join("devices/usb1/1-2");
    fs::create_dir_all(&usb).unwrap();
    std::os::unix::fs::symlink(usb, nic.join("device")).unwrap();
    assert!(
        network_interfaces(vec![interface("eth0", 1)], Some(temp.path()))[0]
            .pci_address
            .is_empty()
    );
}

#[test]
fn pci_inventory_ignores_partial_entries_and_sorts_devices() {
    let temp = tempfile::tempdir().unwrap();
    let later = device(temp.path(), "0000:03:00.0");
    fs::write(later.join("numa_node"), "-1\n").unwrap();
    let first = device(temp.path(), "0000:00:01.0");
    fs::write(first.join("numa_node"), "0\n").unwrap();
    let invalid = device(temp.path(), "0000:04:00.0");
    fs::write(invalid.join("vendor"), "not-hex\n").unwrap();
    let missing = device(temp.path(), "0000:05:00.0");
    fs::remove_file(missing.join("device")).unwrap();
    let signed = device(temp.path(), "0000:06:00.0");
    fs::write(signed.join("vendor"), "+f\n").unwrap();
    device(temp.path(), "not-a-pci-address");
    let devices = pci_devices(temp.path());
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].address, "0000:00:01.0");
    assert_eq!(devices[0].vendor_id, "8086");
    assert_eq!(devices[0].device_id, "1234");
    assert_eq!(devices[0].class_id, "020000");
    assert_eq!(devices[0].numa_node, Some(0));
    assert!(devices[0].driver.is_empty());
    assert_eq!(devices[1].address, "0000:03:00.0");
    assert_eq!(devices[1].numa_node, None);
}

#[test]
#[cfg(unix)]
fn pci_inventory_reads_driver_without_optional_numa() {
    let temp = tempfile::tempdir().unwrap();
    let path = device(temp.path(), "0000:03:00.0");
    std::os::unix::fs::symlink("../../../drivers/iwlwifi", path.join("driver")).unwrap();
    assert_eq!(pci_devices(temp.path())[0].driver, "iwlwifi");
    assert_eq!(pci_devices(temp.path())[0].numa_node, None);
}

#[test]
fn missing_optional_inventory_keeps_native_addresses() {
    let temp = tempfile::tempdir().unwrap();
    assert!(pci_devices(temp.path()).is_empty());
    assert!(os_image(&temp.path().join("os-release")).is_empty());
    let interfaces = network_interfaces(vec![interface("eth0", 1)], Some(temp.path()));
    assert_eq!(interfaces[0].addresses, ["192.0.2.10/24"]);
    assert_eq!(interfaces[0].mtu, 0);
    assert!(interfaces[0].pci_address.is_empty());
}

#[test]
fn os_release_is_literal_and_pretty_name_takes_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("os-release");
    fs::write(
        &path,
        "NAME='Example'\nPRETTY_NAME=\"Example \\\"Linux\\\" \\$(literal)\"\n",
    )
    .unwrap();
    assert_eq!(os_image(&path), "Example \"Linux\" $(literal)");
    fs::write(&path, "# comment\nNAME='Example Linux'\n").unwrap();
    assert_eq!(os_image(&path), "Example Linux");
    fs::write(&path, "PRETTY_NAME=\"unterminated\n").unwrap();
    assert!(os_image(&path).is_empty());
}

#[test]
fn pci_address_checks_every_component() {
    for address in ["0000:03:00.0", "0000:00:1f.7", "00000001:00:00.0"] {
        assert!(pci_address(address), "{address}");
    }
    for address in [
        "1-2",
        "eth0",
        "0000:03:20.0",
        "0000:03:00.8",
        "0000:03:+1.0",
        "0000:03:00.0:1",
    ] {
        assert!(!pci_address(address), "{address}");
    }
}
