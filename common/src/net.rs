use std::collections::HashMap;
use std::net::IpAddr;

use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use regex::Regex;

use crate::FlameError;

/// Network interface addresses grouped by name.
pub type InterfaceList = HashMap<String, Vec<IpAddr>>;

pub fn system_interfaces() -> Result<InterfaceList, FlameError> {
    let mut interfaces = InterfaceList::new();
    for interface in NetworkInterface::show().map_err(|e| FlameError::Network(e.to_string()))? {
        interfaces
            .entry(interface.name)
            .or_default()
            .extend(interface.addr.iter().map(|addr| addr.ip()));
    }
    Ok(interfaces)
}

/// Resolve a literal IP or an interface regex to an IP address. Interface
/// matches use name order and the selected interface's first IPv4 address.
/// `None` means the value is neither an IP nor a matching interface selector;
/// callers decide whether to reject it or interpret it as a hostname/URL.
/// A matched interface without IPv4 is always a configuration error.
pub fn ip_addr(value: &str) -> Result<Option<IpAddr>, FlameError> {
    let literal = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(value);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Ok(Some(ip));
    }
    let Ok(pattern) = Regex::new(value) else {
        return Ok(None);
    };
    interface_ip(&pattern, &system_interfaces()?)
}

fn interface_ip(pattern: &Regex, interfaces: &InterfaceList) -> Result<Option<IpAddr>, FlameError> {
    let Some((_, addresses)) = interfaces
        .iter()
        .filter(|(name, _)| pattern.is_match(name))
        .min_by_key(|(name, _)| name.as_str())
    else {
        return Ok(None);
    };
    addresses
        .iter()
        .copied()
        .find(IpAddr::is_ipv4)
        .map(Some)
        .ok_or_else(|| {
            FlameError::InvalidConfig(format!(
                "network interface <{pattern}> has no IPv4 addresses"
            ))
        })
}

pub fn host_for_uri(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_ips_skip_interface_discovery() {
        for value in ["127.0.0.1", "192.0.2.10", "::1"] {
            assert_eq!(ip_addr(value).unwrap(), Some(value.parse().unwrap()));
        }
        assert_eq!(ip_addr("[::1]").unwrap(), Some("::1".parse().unwrap()));
    }

    #[test]
    fn interface_selection_uses_name_order_and_first_ipv4() {
        let interfaces = InterfaceList::from([
            ("eth1".into(), vec!["192.0.2.11".parse().unwrap()]),
            (
                "eth0".into(),
                vec![
                    "::1".parse().unwrap(),
                    "192.0.2.10".parse().unwrap(),
                    "192.0.2.12".parse().unwrap(),
                ],
            ),
        ]);
        assert_eq!(
            interface_ip(&Regex::new("^eth[01]$").unwrap(), &interfaces).unwrap(),
            Some("192.0.2.10".parse().unwrap())
        );
    }

    #[test]
    fn unresolved_selectors_allow_fallback_and_ipv6_only_interfaces_fail() {
        assert_eq!(ip_addr("[").unwrap(), None);
        assert_eq!(ip_addr("^flame_nonexistent_interface$").unwrap(), None);
        assert_eq!(
            ip_addr("^lo0?$").unwrap(),
            Some("127.0.0.1".parse().unwrap())
        );
        assert!(matches!(
            interface_ip(
                &Regex::new("^eth0$").unwrap(),
                &InterfaceList::from([("eth0".into(), vec!["::1".parse().unwrap()])])
            ),
            Err(FlameError::InvalidConfig(_))
        ));
    }

    #[test]
    fn host_for_uri_brackets_ipv6() {
        assert_eq!(host_for_uri("2001:db8::1"), "[2001:db8::1]");
    }

    #[test]
    fn host_for_uri_leaves_dns_and_bracketed_hosts_unchanged() {
        assert_eq!(host_for_uri("localhost"), "localhost");
        assert_eq!(host_for_uri("[2001:db8::1]"), "[2001:db8::1]");
    }
}
