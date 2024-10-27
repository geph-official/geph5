use std::net::{IpAddr, SocketAddr};

use crate::CONFIG_FILE;

pub fn proxy_allowed(addr: SocketAddr, is_free: bool) -> bool {
    if is_free
        && !CONFIG_FILE
            .wait()
            .free_port_whitelist
            .contains(&addr.port())
    {
        return false;
    }
    is_globally_routable(&addr.ip())
}

fn is_globally_routable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_ipv4_globally_routable(ipv4),
        IpAddr::V6(ipv6) => is_ipv6_globally_routable(ipv6),
    }
}

fn is_ipv4_globally_routable(ip: &std::net::Ipv4Addr) -> bool {
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_broadcast()
        && !ip.is_documentation()
        && !ip.is_unspecified()
        && !is_ipv4_shared_address(ip)
        && !is_ipv4_ietf_protocol_assignment(ip)
        && !is_ipv4_reserved(ip)
        && !is_ipv4_benchmarking(ip)
}

fn is_ipv6_globally_routable(ip: &std::net::Ipv6Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !is_ipv6_unique_local(ip)
        && !is_ipv6_link_local(ip)
}

fn is_ipv4_shared_address(ip: &std::net::Ipv4Addr) -> bool {
    ip.octets()[0] == 100 && (ip.octets()[1] & 0b1100_0000 == 0b0100_0000)
}

fn is_ipv4_ietf_protocol_assignment(ip: &std::net::Ipv4Addr) -> bool {
    ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 0
}

fn is_ipv4_reserved(ip: &std::net::Ipv4Addr) -> bool {
    ip.octets()[0] & 240 == 240 && !ip.is_broadcast()
}

fn is_ipv4_benchmarking(ip: &std::net::Ipv4Addr) -> bool {
    ip.octets()[0] == 198 && (ip.octets()[1] & 0xfe) == 18
}

fn is_ipv6_unique_local(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn is_ipv6_link_local(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}
