use super::{Error, ListenAddr};
use std::{
    io::Write,
    path::PathBuf,
    process::{Child, Command},
};

/// Mentions the type of port forwarding required
#[derive(Debug)]
pub enum PortForwardingType {
    /// Allows a port listening on the local machine to be available on the remote host
    LocalPortToRemote,
    /// Allows a port listening on the remote machine to be available on the local machine
    RemotePortToLocal,
}

/// A port forward to and from a remote host
#[derive(Debug)]
pub struct PortForward {
    child: Child,
}

impl PortForward {
    pub(crate) fn new(
        control_path: PathBuf,
        addr: &str,
        forward_type: PortForwardingType,
        local_addr: ListenAddr,
        remote_addr: ListenAddr,
    ) -> Result<Self, Error> {
        let local = match local_addr {
            ListenAddr::SocketAddr(socket_addr) => socket_addr.to_string(),
            ListenAddr::UnixSocketAddr(socket) => socket,
        };
        let remote = match remote_addr {
            ListenAddr::SocketAddr(socket_addr) => socket_addr.to_string(),
            ListenAddr::UnixSocketAddr(socket) => socket,
        };
        let child = Command::new("ssh")
            .arg("-S")
            .arg(control_path)
            .arg(match forward_type {
                PortForwardingType::LocalPortToRemote => "-L",
                PortForwardingType::RemotePortToLocal => "-R",
            })
            .arg(match forward_type {
                PortForwardingType::LocalPortToRemote => {
                    format!("{}:{}", local, remote)
                }
                PortForwardingType::RemotePortToLocal => {
                    format!("{}:{}", remote, local)
                }
            })
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-O")
            .arg(addr)
            .spawn()
            .map_err(Error::Ssh)?;
        Ok(Self { child })
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        if let Some(ref mut stdin) = self.child.stdin {
            let _ = stdin.write(b"exit\n");
        }
        let _ = self.child.kill();
    }
}
