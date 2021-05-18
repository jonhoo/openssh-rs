use std::net::SocketAddr;

/// Contains a ListenAddr that can either be a unix socket address or an IP address
/// I don't know why this isn't a part of the std library
#[derive(Debug)]
pub enum ListenAddr {
    /// An SocketAddr that corresponds to this [`ListenAddr`]
    SocketAddr(SocketAddr),
    /// A Unix Socket address that this [`ListenAddr`] corresponds to
    UnixSocketAddr(String),
}

impl From<SocketAddr> for ListenAddr {
    fn from(socketaddr: SocketAddr) -> Self {
        ListenAddr::SocketAddr(socketaddr)
    }
}

impl From<String> for ListenAddr {
    fn from(socket: String) -> Self {
        ListenAddr::UnixSocketAddr(socket)
    }
}

impl PartialEq<ListenAddr> for SocketAddr {
    fn eq(&self, other: &ListenAddr) -> bool {
        match other {
            ListenAddr::SocketAddr(ip) => self == ip,
            ListenAddr::UnixSocketAddr(_) => false,
        }
    }
}

impl PartialEq<ListenAddr> for String {
    fn eq(&self, other: &ListenAddr) -> bool {
        match other {
            ListenAddr::SocketAddr(_) => false,
            ListenAddr::UnixSocketAddr(path) => path == other,
        }
    }
}
