use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Errors returned by SSRF validation.
#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("Blocked scheme: {0}")]
    BlockedScheme(String),
    #[error("Blocked internal IP: {0}")]
    BlockedInternalIp(String),
    #[error("Blocked metadata endpoint: {0}")]
    BlockedMetadata(String),
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Missing host in URL")]
    MissingHost,
}

/// SSRF protection — validates URLs to block requests to internal/dangerous targets.
#[derive(Debug, Clone)]
pub struct SsrfValidator {
    /// Allowlisted internal URLs (exact prefix match).
    allowed_internal: Vec<String>,
}

/// Blocked URL schemes.
const BLOCKED_SCHEMES: &[&str] = &["file://", "data:", "gopher://", "ftp://"];

/// Blocked metadata hostnames.
const BLOCKED_METADATA_HOSTS: &[&str] = &["169.254.169.254", "metadata.google.internal"];

impl SsrfValidator {
    /// Create a new SSRF validator with allowlisted internal URLs.
    pub fn new(allowed_internal: Vec<String>) -> Self {
        Self { allowed_internal }
    }

    /// Create a validator with no allowlisted internal URLs.
    pub fn strict() -> Self {
        Self {
            allowed_internal: Vec::new(),
        }
    }

    /// Validate a URL, blocking internal IPs, dangerous schemes, and metadata endpoints.
    pub fn validate_url(&self, url: &str) -> Result<(), SsrfError> {
        let trimmed = url.trim();

        // Check allowlist first (exact prefix match)
        for allowed in &self.allowed_internal {
            if trimmed.starts_with(allowed.as_str()) {
                return Ok(());
            }
        }

        // Block dangerous schemes
        let lower = trimmed.to_lowercase();
        for scheme in BLOCKED_SCHEMES {
            if lower.starts_with(scheme) {
                tracing::warn!(url = trimmed, "SSRF: blocked scheme");
                return Err(SsrfError::BlockedScheme(scheme.to_string()));
            }
        }

        // Must be http or https
        if !lower.starts_with("http://") && !lower.starts_with("https://") {
            return Err(SsrfError::BlockedScheme(
                "Only http:// and https:// allowed".to_string(),
            ));
        }

        // Extract host portion
        let host = extract_host(trimmed)?;

        // Check metadata endpoints
        for meta in BLOCKED_METADATA_HOSTS {
            if host.eq_ignore_ascii_case(meta) {
                tracing::warn!(
                    url = trimmed,
                    host = host.as_str(),
                    "SSRF: blocked metadata endpoint"
                );
                return Err(SsrfError::BlockedMetadata(host));
            }
        }

        // Try parsing as IP address and check ranges
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_internal_ip(&ip) {
                tracing::warn!(url = trimmed, %ip, "SSRF: blocked internal IP");
                return Err(SsrfError::BlockedInternalIp(ip.to_string()));
            }
        }

        // Handle bracket-wrapped IPv6 like [::1]
        if host.starts_with('[') && host.ends_with(']') {
            let inner = &host[1..host.len() - 1];
            if let Ok(ip) = inner.parse::<Ipv6Addr>() {
                if is_internal_ip(&IpAddr::V6(ip)) {
                    tracing::warn!(url = trimmed, %ip, "SSRF: blocked internal IPv6");
                    return Err(SsrfError::BlockedInternalIp(ip.to_string()));
                }
            }
        }

        Ok(())
    }
}

/// Extract the host portion from an HTTP(S) URL.
fn extract_host(url: &str) -> Result<String, SsrfError> {
    // Strip scheme
    let after_scheme = url
        .find("://")
        .map(|i| &url[i + 3..])
        .ok_or(SsrfError::InvalidUrl("no scheme separator".to_string()))?;

    // Strip path/query
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);

    // Strip userinfo (user:pass@host)
    let after_userinfo = if let Some(at_pos) = authority.rfind('@') {
        &authority[at_pos + 1..]
    } else {
        authority
    };

    // Handle IPv6 bracket notation
    if after_userinfo.starts_with('[') {
        // Find closing bracket
        if let Some(bracket_end) = after_userinfo.find(']') {
            return Ok(after_userinfo[..=bracket_end].to_string());
        }
        return Err(SsrfError::InvalidUrl("unclosed IPv6 bracket".to_string()));
    }

    // Strip port
    let host = if let Some(colon_pos) = after_userinfo.rfind(':') {
        &after_userinfo[..colon_pos]
    } else {
        after_userinfo
    };

    if host.is_empty() {
        return Err(SsrfError::MissingHost);
    }

    Ok(host.to_string())
}

/// Check if an IP address is internal/private/reserved.
fn is_internal_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_internal_ipv4(v4),
        IpAddr::V6(v6) => is_internal_ipv6(v6),
    }
}

fn is_internal_ipv4(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();

    // Loopback: 127.0.0.0/8
    if octets[0] == 127 {
        return true;
    }
    // RFC 1918: 10.0.0.0/8
    if octets[0] == 10 {
        return true;
    }
    // RFC 1918: 172.16.0.0/12
    if octets[0] == 172 && (16..=31).contains(&octets[1]) {
        return true;
    }
    // RFC 1918: 192.168.0.0/16
    if octets[0] == 192 && octets[1] == 168 {
        return true;
    }
    // Link-local: 169.254.0.0/16
    if octets[0] == 169 && octets[1] == 254 {
        return true;
    }
    // Broadcast
    if octets == [255, 255, 255, 255] {
        return true;
    }
    // Unspecified
    if octets == [0, 0, 0, 0] {
        return true;
    }

    false
}

fn is_internal_ipv6(ip: &Ipv6Addr) -> bool {
    // Loopback: ::1
    if ip == &Ipv6Addr::LOCALHOST {
        return true;
    }
    // Unspecified: ::
    if ip == &Ipv6Addr::UNSPECIFIED {
        return true;
    }
    let segments = ip.segments();
    // Link-local: fe80::/10
    if segments[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    // Unique local: fc00::/7
    if segments[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    // IPv4-mapped: ::ffff:x.x.x.x — check the mapped IPv4
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_internal_ipv4(&v4);
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allows_public_http() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("https://example.com/api").is_ok());
        assert!(v.validate_url("http://8.8.8.8/dns").is_ok());
    }

    #[test]
    fn test_blocks_file_scheme() {
        let v = SsrfValidator::strict();
        let err = v.validate_url("file:///etc/passwd").unwrap_err();
        assert!(matches!(err, SsrfError::BlockedScheme(_)));
    }

    #[test]
    fn test_blocks_data_scheme() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("data:text/html,<h1>hi</h1>").is_err());
    }

    #[test]
    fn test_blocks_gopher_scheme() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("gopher://evil.com").is_err());
    }

    #[test]
    fn test_blocks_ftp_scheme() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("ftp://files.example.com").is_err());
    }

    #[test]
    fn test_blocks_rfc1918_10() {
        let v = SsrfValidator::strict();
        let err = v.validate_url("http://10.1.2.3/admin").unwrap_err();
        assert!(matches!(err, SsrfError::BlockedInternalIp(_)));
    }

    #[test]
    fn test_blocks_rfc1918_172() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("http://172.16.0.1/admin").is_err());
        assert!(v.validate_url("http://172.31.255.255/admin").is_err());
        // 172.15 and 172.32 are public
        assert!(v.validate_url("http://172.15.0.1/admin").is_ok());
        assert!(v.validate_url("http://172.32.0.1/admin").is_ok());
    }

    #[test]
    fn test_blocks_rfc1918_192_168() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("http://192.168.1.1/admin").is_err());
    }

    #[test]
    fn test_blocks_loopback() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("http://127.0.0.1:8080/api").is_err());
        assert!(v.validate_url("http://127.0.0.2/api").is_err());
    }

    #[test]
    fn test_blocks_link_local() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("http://169.254.1.1/api").is_err());
    }

    #[test]
    fn test_blocks_aws_metadata() {
        let v = SsrfValidator::strict();
        let err = v
            .validate_url("http://169.254.169.254/latest/meta-data/")
            .unwrap_err();
        assert!(matches!(err, SsrfError::BlockedMetadata(_)));
    }

    #[test]
    fn test_blocks_google_metadata() {
        let v = SsrfValidator::strict();
        let err = v
            .validate_url("http://metadata.google.internal/computeMetadata/v1/")
            .unwrap_err();
        assert!(matches!(err, SsrfError::BlockedMetadata(_)));
    }

    #[test]
    fn test_blocks_ipv6_loopback() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("http://[::1]:8080/api").is_err());
    }

    #[test]
    fn test_blocks_ipv6_link_local() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("http://[fe80::1]/api").is_err());
    }

    #[test]
    fn test_blocks_ipv6_unique_local() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("http://[fd00::1]/api").is_err());
    }

    #[test]
    fn test_allowlist_overrides_block() {
        let v = SsrfValidator::new(vec![
            "http://10.1.2.4:8080".to_string(),
            "http://127.0.0.1:18790".to_string(),
        ]);
        assert!(v.validate_url("http://10.1.2.4:8080/search").is_ok());
        assert!(v.validate_url("http://127.0.0.1:18790/api").is_ok());
        // Other internal IPs still blocked
        assert!(v.validate_url("http://10.0.0.3/admin").is_err());
    }

    #[test]
    fn test_unknown_scheme_blocked() {
        let v = SsrfValidator::strict();
        assert!(v.validate_url("ldap://dc.internal").is_err());
    }

    #[test]
    fn test_extract_host_with_port() {
        let host = extract_host("http://example.com:8080/path").unwrap();
        assert_eq!(host, "example.com");
    }

    #[test]
    fn test_extract_host_with_userinfo() {
        let host = extract_host("http://user:pass@example.com/path").unwrap();
        assert_eq!(host, "example.com");
    }

    #[test]
    fn test_missing_host() {
        assert!(extract_host("http:///path").is_err());
    }
}
