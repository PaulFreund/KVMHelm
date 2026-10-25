//! Pure LAN and ICE guards. A transport must enforce these on every resolved
//! address/connection; validating only the configured hostname is insufficient.
use crate::{GatewayError, Result};
use std::net::{IpAddr, Ipv4Addr};
use url::Url;

fn local_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 10
        || a == 127
        || (a == 192 && b == 168)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 169 && b == 254)
}

pub fn is_local_address(value: &str) -> bool {
    match value.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => local_v4(ip),
        Ok(IpAddr::V6(ip)) => ip.to_ipv4_mapped().map(local_v4).unwrap_or_else(|| {
            ip.is_loopback()
                || ip.segments()[0] & 0xfe00 == 0xfc00
                || ip.segments()[0] & 0xffc0 == 0xfe80
        }),
        Err(_) => false,
    }
}

pub fn check_resolved_addresses(addresses: &[IpAddr]) -> Result<()> {
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|ip| !is_local_address(&ip.to_string()))
    {
        return Err(GatewayError::message(
            "INVALID_ADDRESS",
            "KVM targets must resolve only to local/LAN addresses",
        ));
    }
    Ok(())
}

pub fn device_origin(address: &str) -> Result<Url> {
    let url = Url::parse(address).map_err(|_| GatewayError::new("INVALID_ADDRESS"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        return Err(GatewayError::message(
            "INVALID_ADDRESS",
            "Use an HTTP(S) origin without credentials or path",
        ));
    }
    // Literal IPs can be rejected immediately; DNS must be checked at connect.
    if let Some(host) = url.host_str() {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.parse::<IpAddr>().is_ok() && !is_local_address(host) {
            return Err(GatewayError::new("INVALID_ADDRESS"));
        }
    }
    Ok(url)
}

pub fn check_ice_candidate(candidate: &str) -> Result<()> {
    if !candidate
        .split_whitespace()
        .nth(4)
        .is_some_and(is_local_address)
    {
        return Err(GatewayError::message(
            "INVALID_ADDRESS",
            "ICE must use local/LAN addresses",
        ));
    }
    Ok(())
}

pub fn check_ice_description(sdp: &str) -> Result<()> {
    for line in sdp.lines() {
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            check_ice_candidate(&format!("candidate:{candidate}"))?;
        }
    }
    Ok(())
}

pub fn local_ice_description(sdp: &str) -> String {
    sdp.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.starts_with("a=candidate:") || check_ice_candidate(&line[2..]).is_ok())
        .collect::<Vec<_>>()
        .join("\r\n")
}
