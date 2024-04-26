use bytes::{Buf, BufMut, BytesMut};
use std::{
    fmt::{self, Debug},
    io::Cursor,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};
use tokio::io::{self, AsyncRead, AsyncReadExt};
/// SOCKS5 protocol error

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Address {
    SocketAddress(SocketAddr),
    DomainNameAddress(String, u16),
}

impl Debug for Address {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter) -> fmt::Result {
        match *self {
            Address::SocketAddress(ref addr) => write!(f, "{}", addr),
            Address::DomainNameAddress(ref addr, ref port) => write!(f, "{} {}", addr, port),
        }
    }
}
impl fmt::Display for Address {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter) -> fmt::Result {
        match *self {
            Address::SocketAddress(ref addr) => write!(f, "{}", addr),
            Address::DomainNameAddress(ref addr, ref port) => write!(f, "{}:{}", addr, port),
        }
    }
}
impl std::net::ToSocketAddrs for Address {
    type Iter = std::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<std::vec::IntoIter<SocketAddr>> {
        match self.clone() {
            Address::SocketAddress(addr) => Ok(vec![addr].into_iter()),
            Address::DomainNameAddress(addr, port) => (&addr[..], port).to_socket_addrs(),
        }
    }
}
impl From<SocketAddr> for Address {
    fn from(s: SocketAddr) -> Address {
        Address::SocketAddress(s)
    }
}
impl From<(String, u16)> for Address {
    fn from((dn, port): (String, u16)) -> Address {
        Address::DomainNameAddress(dn, port)
    }
}
#[inline]
fn get_addr_len(atyp: &Address) -> usize {
    match *atyp {
        Address::SocketAddress(SocketAddr::V4(..)) => 1 + 4 + 2,
        Address::SocketAddress(SocketAddr::V6(..)) => 1 + 8 * 2 + 2,
        Address::DomainNameAddress(ref dmname, _) => 1 + 1 + dmname.len() + 2,
    }
}

pub fn host_addr(uri: &hyper::Uri) -> Option<Address> {
    match uri.authority() {
        None => None,
        Some(authority) => {
            // NOTE: Authority may include authentication info (user:password)
            // Although it is already deprecated, but some very old application may still depending on it
            //
            // But ... We won't be compatible with it. :)

            // Check if URI has port
            match authority.port_u16() {
                Some(port) => {
                    // Well, it has port!
                    // 1. Maybe authority is a SocketAddr (127.0.0.1:1234, [::1]:1234)
                    // 2. Otherwise, it must be a domain name (google.com:443)

                    match authority.as_str().parse::<SocketAddr>() {
                        Ok(saddr) => Some(Address::from(saddr)),
                        Err(..) => Some(Address::DomainNameAddress(
                            authority.host().to_owned(),
                            port,
                        )),
                    }
                }
                None => {
                    // Ok, we don't have port
                    // 1. IPv4 Address 127.0.0.1
                    // 2. IPv6 Address: https://tools.ietf.org/html/rfc2732 , [::1]
                    // 3. Domain name

                    // Uses default port
                    let port = match uri.scheme_str() {
                        None => 80, // Assume it is http
                        Some("http") => 80,
                        Some("https") => 443,
                        _ => return None, // Not supported
                    };

                    // RFC2732 indicates that IPv6 address should be wrapped in [ and ]
                    let authority_str = authority.as_str();
                    if authority_str.starts_with('[') && authority_str.ends_with(']') {
                        // Must be a IPv6 address
                        let addr = authority_str.trim_start_matches('[').trim_end_matches(']');
                        match addr.parse::<std::net::IpAddr>() {
                            Ok(a) => Some(Address::from(SocketAddr::new(a, port))),
                            // Ignore invalid IPv6 address
                            Err(..) => None,
                        }
                    } else {
                        // Maybe it is a IPv4 address, or a non-standard IPv6
                        match authority_str.parse::<std::net::IpAddr>() {
                            Ok(a) => Some(Address::from(SocketAddr::new(a, port))),
                            // Should be a domain name, or a invalid IP address.
                            // Let DNS deal with it.
                            Err(..) => {
                                Some(Address::DomainNameAddress(authority_str.to_owned(), port))
                            }
                        }
                    }
                }
            }
        }
    }
}
