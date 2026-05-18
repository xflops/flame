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
    fn host_for_uri_brackets_ipv6() {
        assert_eq!(host_for_uri("2001:db8::1"), "[2001:db8::1]");
    }

    #[test]
    fn host_for_uri_leaves_dns_and_bracketed_hosts_unchanged() {
        assert_eq!(host_for_uri("localhost"), "localhost");
        assert_eq!(host_for_uri("[2001:db8::1]"), "[2001:db8::1]");
    }
}
