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

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::Path;

use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};

use super::{NodeInfo, NodeNetworkInterface, NodePciDevice};

impl NodeInfo {
    /// Collects visible inventory without making optional hardware a
    /// prerequisite for registration. FEM runs this outside its node lock.
    pub fn discover() -> Self {
        let sysfs = cfg!(target_os = "linux").then(|| Path::new("/sys"));
        let interfaces = NetworkInterface::show().unwrap_or_else(|error| {
            tracing::debug!(%error, "Network interface inventory unavailable");
            Vec::new()
        });
        Self {
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            kernel_version: kernel_version(),
            os_image: if cfg!(target_os = "linux") {
                let path = Path::new("/etc/os-release");
                let path = if matches!(path.try_exists(), Ok(false)) {
                    Path::new("/usr/lib/os-release")
                } else {
                    path
                };
                os_image(path)
            } else {
                String::new()
            },
            network_interfaces: network_interfaces(interfaces, sysfs),
            pci_devices: sysfs.map(pci_devices).unwrap_or_default(),
        }
    }
}

/// Returns the Linux kernel release without executing a system utility.
#[cfg(target_os = "linux")]
fn kernel_version() -> String {
    rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned()
}

/// Leaves Linux-specific kernel metadata unavailable on other platforms.
#[cfg(not(target_os = "linux"))]
fn kernel_version() -> String {
    String::new()
}

/// Treats disappearing or inaccessible optional attributes as missing data.
fn attribute(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Groups assigned addresses by interface and optionally adds Linux metadata.
fn network_interfaces(
    interfaces: Vec<NetworkInterface>,
    sysfs: Option<&Path>,
) -> Vec<NodeNetworkInterface> {
    let mut grouped = BTreeMap::<String, NodeNetworkInterface>::new();
    for interface in interfaces {
        let entry = grouped
            .entry(interface.name.clone())
            .or_insert_with(|| NodeNetworkInterface {
                name: interface.name,
                index: interface.index,
                ..Default::default()
            });
        if entry.mac_address.is_empty() {
            entry.mac_address = interface.mac_addr.unwrap_or_default().to_ascii_lowercase();
        }
        entry
            .addresses
            .extend(interface.addr.into_iter().map(address));
    }
    grouped
        .into_values()
        .map(|mut interface| {
            interface.addresses.sort();
            interface.addresses.dedup();
            if let Some(sysfs) = sysfs {
                let path = sysfs.join("class/net").join(&interface.name);
                if let Some(mac) = attribute(&path.join("address")) {
                    interface.mac_address = mac.to_ascii_lowercase();
                }
                interface.mtu = attribute(&path.join("mtu"))
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default();
                interface.state = attribute(&path.join("operstate")).unwrap_or_default();
                interface.pci_address = fs::canonicalize(path.join("device"))
                    .ok()
                    .and_then(|path| path.file_name()?.to_str().map(str::to_string))
                    .filter(|name| pci_address(name))
                    .map(|name| name.to_ascii_lowercase())
                    .unwrap_or_default();
            }
            interface
        })
        .collect()
}

/// Adds a CIDR prefix only for a present, contiguous netmask.
fn address(addr: Addr) -> String {
    let prefix = addr.netmask().and_then(|mask| match mask {
        IpAddr::V4(mask) => {
            let mask = u32::from(mask);
            let ones = mask.leading_ones();
            (ones == mask.count_ones()).then_some(ones)
        }
        IpAddr::V6(mask) => {
            let mask = u128::from(mask);
            let ones = mask.leading_ones();
            (ones == mask.count_ones()).then_some(ones)
        }
    });
    match prefix {
        Some(prefix) => format!("{}/{prefix}", addr.ip()),
        None => addr.ip().to_string(),
    }
}

/// Validates a PCI domain:bus:device.function identity, excluding USB paths.
fn pci_address(value: &str) -> bool {
    let mut parts = value.split(':');
    let Some(domain) = parts.next() else {
        return false;
    };
    let Some(bus) = parts.next() else {
        return false;
    };
    let Some((device, function)) = parts.next().and_then(|part| part.split_once('.')) else {
        return false;
    };
    parts.next().is_none()
        && (4..=8).contains(&domain.len())
        && domain.bytes().all(|byte| byte.is_ascii_hexdigit())
        && bus.len() == 2
        && bus.bytes().all(|byte| byte.is_ascii_hexdigit())
        && device.len() == 2
        && device.bytes().all(|byte| byte.is_ascii_hexdigit())
        && u8::from_str_radix(device, 16).is_ok_and(|value| value < 32)
        && function.len() == 1
        && function.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
}

/// Normalizes a PCI ID to fixed-width lowercase hex without its 0x prefix.
fn pci_id(path: &Path, digits: usize) -> Option<String> {
    let value = attribute(path)?;
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(&value);
    if value.is_empty()
        || value.len() > digits
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(value, 16).ok()?;
    Some(format!("{value:0digits$x}"))
}

/// Reads PCI IDs and optional driver/NUMA information without requiring lspci.
fn pci_devices(sysfs: &Path) -> Vec<NodePciDevice> {
    let Ok(entries) = fs::read_dir(sysfs.join("bus/pci/devices")) else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !pci_address(&name) {
            continue;
        }
        let path = entry.path();
        let Some(vendor_id) = pci_id(&path.join("vendor"), 4) else {
            continue;
        };
        let Some(device_id) = pci_id(&path.join("device"), 4) else {
            continue;
        };
        let Some(class_id) = pci_id(&path.join("class"), 6) else {
            continue;
        };
        let driver = fs::read_link(path.join("driver"))
            .ok()
            .and_then(|path| path.file_name()?.to_str().map(str::to_string))
            .unwrap_or_default();
        let numa_node = attribute(&path.join("numa_node"))
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|value| *value >= 0);
        devices.push(NodePciDevice {
            address: name.to_ascii_lowercase(),
            vendor_id,
            device_id,
            class_id,
            driver,
            numa_node,
        });
    }
    devices.sort_by(|left, right| left.address.cmp(&right.address));
    devices.dedup_by(|left, right| left.address == right.address);
    devices
}

/// Reads display-only os-release values without evaluating shell code.
fn os_image(path: &Path) -> String {
    let Ok(contents) = fs::read_to_string(path) else {
        return String::new();
    };
    let mut pretty_name = None;
    let mut name = None;
    for line in contents.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match key.trim() {
            "PRETTY_NAME" => pretty_name = os_value(value.trim()),
            "NAME" => name = os_value(value.trim()),
            _ => {}
        }
    }
    pretty_name.or(name).unwrap_or_default()
}

/// Removes os-release quoting and escapes while keeping substitutions literal.
fn os_value(value: &str) -> Option<String> {
    if let Some(value) = value.strip_prefix('\'') {
        return value.strip_suffix('\'').map(str::to_string);
    }
    let Some(value) = value.strip_prefix('"') else {
        return Some(value.to_string());
    };
    let value = value.strip_suffix('"')?;
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && chars
                .peek()
                .is_some_and(|next| matches!(next, '\\' | '"' | '$' | '`'))
        {
            output.push(chars.next()?);
        } else {
            output.push(ch);
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests;
