use super::{Error, Session};
use std::io::prelude::*;
use std::process::{self, Stdio};
use tempfile::Builder;

/// Build a [`Session`] with options.
#[derive(Debug, Clone)]
pub struct SessionBuilder {
    user: Option<String>,
    port: Option<String>,
    keyfile: Option<std::path::PathBuf>,
    connect_timeout: Option<String>,
    known_hosts_check: KnownHosts,
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            user: None,
            port: None,
            keyfile: None,
            connect_timeout: None,
            known_hosts_check: KnownHosts::Add,
        }
    }
}

impl SessionBuilder {
    /// Set the ssh user (`ssh -l`).
    ///
    /// Defaults to `None`.
    pub fn user(&mut self, user: String) -> &mut Self {
        self.user = Some(user);
        self
    }

    /// Set the port to connect on (`ssh -p`).
    ///
    /// Defaults to `None`.
    pub fn port(&mut self, port: u16) -> &mut Self {
        self.port = Some(format!("{}", port));
        self
    }

    /// Set the keyfile to use (`ssh -i`).
    ///
    /// Defaults to `None`.
    pub fn keyfile(&mut self, p: impl AsRef<std::path::Path>) -> &mut Self {
        self.keyfile = Some(p.as_ref().to_path_buf());
        self
    }

    /// See [`KnownHosts`].
    ///
    /// Default `KnownHosts::Add`.
    pub fn known_hosts_check(&mut self, k: KnownHosts) -> &mut Self {
        self.known_hosts_check = k;
        self
    }

    /// Set the connection timeout (`ssh -o ConnectTimeout`).
    ///
    /// This value is specified in seconds. Any sub-second duration remainder will be ignored.
    /// Defaults to `None`.
    pub fn connect_timeout(&mut self, d: std::time::Duration) -> &mut Self {
        self.connect_timeout = Some(d.as_secs().to_string());
        self
    }

    /// Connect to the host at the given `host` over SSH.
    ///
    /// The format of `destination` is the same as the `destination` argument to `ssh`. It may be
    /// specified as either `[user@]hostname` or a URI of the form `ssh://[user@]hostname[:port]`.
    /// A username or port that is specified in the connection string overrides the one set in the
    /// builder (but does not change the builder).
    ///
    /// If connecting requires interactive authentication based on `STDIN` (such as reading a
    /// password), the connection will fail. Consider setting up keypair-based authentication
    /// instead.
    pub fn connect<S: AsRef<str>>(&self, destination: S) -> Result<Session, Error> {
        let mut destination = destination.as_ref();

        // the "new" ssh://user@host:port form is not supported by all versions of ssh, so we
        // always translate it into the option form.
        let mut user = None;
        let mut port = None;
        if destination.starts_with("ssh://") {
            destination = &destination[6..];
            if let Some(at) = destination.find('@') {
                // specified a username -- extract it:
                user = Some(&destination[..at]);
                destination = &destination[(at + 1)..];
            }
            if let Some(colon) = destination.rfind(':') {
                let p = &destination[(colon + 1)..];
                if let Ok(p) = p.parse() {
                    // user specified a port -- extract it:
                    port = Some(p);
                    destination = &destination[..colon];
                }
            }
        }

        if user.is_none() && port.is_none() {
            return self.just_connect(destination);
        }

        let mut with_overrides = self.clone();
        if let Some(user) = user {
            with_overrides.user(user.to_owned());
        }

        if let Some(port) = port {
            with_overrides.port(port);
        }

        with_overrides.just_connect(destination)
    }

    pub(crate) fn just_connect<S: AsRef<str>>(&self, host: S) -> Result<Session, Error> {
        let destination = host.as_ref();
        let dir = Builder::new()
            .prefix(".ssh-connection")
            .tempdir_in("./")
            .map_err(Error::Master)?;
        let mut init = process::Command::new("ssh");

        init.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("-S")
            .arg(dir.path().join("master"))
            .arg("-M")
            .arg("-f")
            .arg("-N")
            .arg("-o")
            .arg("ControlPersist=yes")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(self.known_hosts_check.as_option());

        if let Some(ref timeout) = self.connect_timeout {
            init.arg("-o").arg(format!("ConnectTimeout={}", timeout));
        }

        if let Some(ref port) = self.port {
            init.arg("-p").arg(port);
        }

        if let Some(ref user) = self.user {
            init.arg("-l").arg(user);
        }

        if let Some(ref k) = self.keyfile {
            init.arg("-i").arg(k);
        }

        init.arg(destination);

        // eprintln!("{:?}", init);

        // we spawn and immediately wait, because the process is supposed to fork.
        // note that we cannot use .output, since it _also_ tries to read all of stdout/stderr.
        // if the call _didn't_ error, then the backgrounded ssh client will still hold onto those
        // handles, and it's still running, so those reads will hang indefinitely.
        let mut child = init.spawn().map_err(Error::Connect)?;
        let status = child.wait().map_err(Error::Connect)?;

        if let Some(255) = status.code() {
            // this is the ssh command's way of telling us that the connection failed
            let mut stderr = String::new();
            child
                .stderr
                .as_mut()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();

            return Err(Error::interpret_ssh_error(&stderr));
        }

        Ok(Session {
            ctl: dir,
            addr: String::from(destination),
            terminated: false,
            master: std::sync::Mutex::new(Some(child)),
        })
    }
}

/// Specifies how the host's key fingerprint should be handled.
#[derive(Debug, Clone)]
pub enum KnownHosts {
    /// The host's fingerprint must match what is in the known hosts file.
    ///
    /// If the host is not in the known hosts file, the connection is rejected.
    ///
    /// This corresponds to `ssh -o StrictHostKeyChecking=yes`.
    Strict,
    /// Strict, but if the host is not already in the known hosts file, it will be added.
    ///
    /// This corresponds to `ssh -o StrictHostKeyChecking=accept-new`.
    Add,
    /// Accept whatever key the server provides and add it to the known hosts file.
    ///
    /// This corresponds to `ssh -o StrictHostKeyChecking=no`.
    Accept,
}

impl KnownHosts {
    fn as_option(&self) -> &'static str {
        match *self {
            KnownHosts::Strict => "StrictHostKeyChecking=yes",
            KnownHosts::Add => "StrictHostKeyChecking=accept-new",
            KnownHosts::Accept => "StrictHostKeyChecking=no",
        }
    }
}
