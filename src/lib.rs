//! Scriptable SSH through OpenSSH.
//!
//! This crate wraps the OpenSSH remote login client (`ssh` on most machines), and provides
//! a convenient mechanism for running commands on remote hosts. Since all commands are executed
//! through the `ssh` command, all your existing configuration (e.g., in `.ssh/config`) should
//! continue to work as expected.
//!
//! The library's API is modeled closely after that of [`std::process::Command`], since `ssh` also
//! attempts to make the remote process seem as much as possible like a local command. However,
//! there are some differences.
//!
//! First of all, all remote commands are executed in the context of a single ssh
//! [session](Session). Authentication happens once when the session is
//! [established](Session::connect), and subsequent command invocations re-use the same connection.
//! Behind the scenes, the crate uses ssh's [`ControlMaster`] feature to multiplex the channels for
//! the different remote commands. Because of this, each remote command is tied to the lifetime of
//! the [`Session`] that spawned them. When the session is [closed](Session::close), the connection
//! is severed, and there can be no outstanding remote clients.
//!
//! Note that the maximum number of multiplexed remote commands is 10 by default. This value can be
//! increased by changing the `MaxSessions` setting in [`sshd_config`].
//!
//! Much like with [`std::process::Command`], you have multiple options when it comes to launching
//! a remote command. You can [spawn](Command::spawn) the remote command, which just gives you a
//! handle to the running process, you can run the command and wait for its
//! [output](Command::output), or you can run it and just extract its [exit
//! status](Command::status). Unlike its `std` counterpart though, these methods on [`Command`] can
//! fail even if the remote command executed successfully, since there is a fallible network
//! separating you from it.
//!
//! Also unlike its `std` counterpart, [`spawn`](Command::spawn) gives you a [`RemoteChild`] rather
//! than a [`std::process::Child`]. Behind the scenes, a remote child is really just a process
//! handle to the _local_ `ssh` instance corresponding to the spawned remote command. The behavior
//! of the methods of [`RemoteChild`] therefore match the behavior of `ssh`, rather than that of
//! the remote command directly. Usually, these are the same, though not always, as highlighted in
//! the documetantation the individual methods. See also the section below on Remote Shells.
//!
//! And finally, our commands never default to inheriting stdin/stdout/stderr, since we expect you
//! are using this to automate things. Instead, unless otherwise noted, all I/O ports default to
//! [`Stdio::null`](std::process::Stdio::null).
//!
//! # Authentication
//!
//! This library supports only password-less authentication schemes. If running `ssh` to a target
//! host requires you to provide input on standard input (`STDIN`), then this crate will not work
//! for you. You should set up keypair-based authentication instead.
//!
//! # Errors
//!
//! Since we are wrapping the `ssh`, which in turn runs a remote command that we do not control, we
//! do not have a reliable way to tell the difference between what is a failure of the SSH
//! connection itself, and what is a program error from the remote host. We do our best with some
//! heuristics (like `ssh` exiting with status code 255 if a connection error occurs), but the
//! errors from this crate will almost necessarily be worse than those of a native SSH
//! implementation. Sorry in advance :)
//!
//! This also means that you may see strange errors when the remote process is terminated by a
//! signal (such as through `kill` or `pkill`). When this happens, all the local ssh program sees
//! is that the remote process disappeared, and so it returns with an error. It does not
//! communicate that the process exited due to a signal. In cases like this, your call will return
//! [`Error::Disconnected`], because the connection to _that_ remote process was disconnected. The
//! ssh connection as a whole is likely still intact.
//!
//! To check if the connection has truly failed, use [`Session::check`]. It will return `Ok` if the
//! master connection is still operational, and _may_ provide you with more information than you
//! got from the failing command (that is, just [`Error::Disconnected`]) if it is not.
//!
//! # Remote Shells
//!
//! When you invoke a remote command through ssh, the remote command is executed by a shell on the
//! remote end. That shell _interprets_ anything passed to it — it might evalute words starting
//! with `$` as variables, split arguments by whitespace, and other things a shell is wont to do.
//! Since that is _usually_ not what you expect to happen, `.arg("a b")` should pass a _single_
//! argument with the value `a b`, `openssh` _escapes_ every argument (and the command itself) by
//! default using [`shell-escape`]. This works well in most cases, but might run into issues when
//! the remote shell (generally the remote user's login shell) has a different syntax than the
//! shell `shell-escape` targets (bash). For example, Windows shells have different escaping syntax
//! than bash does.
//!
//! If this applies to you, you can use [`raw_arg`](Command::raw_arg),
//! [`raw_args`](Command::raw_args), and [`raw_command`](Session::raw_command) to bypass the
//! escaping that `openssh` normally does for you.
//!
//! # Examples
//!
//! ```rust,no_run
//! # #[tokio::main]
//! # async fn main() -> Result<(), openssh::Error> {
//! use openssh::{Session, KnownHosts};
//!
//! let session = Session::connect("me@ssh.example.com", KnownHosts::Strict).await?;
//! let ls = session.command("ls").output().await?;
//! eprintln!("{}", String::from_utf8(ls.stdout).expect("server output was not valid UTF-8"));
//!
//! let whoami = session.command("whoami").output().await?;
//! assert_eq!(whoami.stdout, b"me\n");
//!
//! session.close().await?;
//! # Ok(()) }
//! ```
//!
//!   [`ControlMaster`]: https://en.wikibooks.org/wiki/OpenSSH/Cookbook/Multiplexing
//!   [`sshd_config`]: https://linux.die.net/man/5/sshd_config
//!   [`shell-escape`]: https://crates.io/crates/shell-escape

#![warn(
    missing_docs,
    missing_debug_implementations,
    broken_intra_doc_links,
    rust_2018_idioms,
    unreachable_pub
)]

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io;
use tokio::io::AsyncReadExt;
use tokio::process;

mod builder;
pub use builder::{KnownHosts, SessionBuilder};

mod command;
pub use command::Command;

mod child;
pub use child::RemoteChild;

mod error;
pub use error::Error;

mod sftp;
pub use sftp::{Mode, RemoteFile, Sftp};

mod port_forwarding;
pub use port_forwarding::{PortForward, PortForwardingType};

mod listen_addr;
pub use listen_addr::ListenAddr;

/// A single SSH session to a remote host.
///
/// You can use [`command`] to start a new command on the connected machine.
///
/// When the `Session` is dropped, the connection to the remote host is severed, and any errors
/// silently ignored. To disconnect and be alerted to errors, use [`close`](Session::close).
#[derive(Debug)]
pub struct Session {
    ctl: tempfile::TempDir,
    addr: String,
    terminated: bool,
    master: std::sync::Mutex<Option<(tokio::process::ChildStdout, tokio::process::ChildStderr)>>,
}

// TODO: UserKnownHostsFile for custom known host fingerprint.
// TODO: Extract process output in Session::check(), Session::connect(), and Session::terminate().

impl Session {
    fn ctl_path(&self) -> std::path::PathBuf {
        self.ctl.path().join("master")
    }

    /// Connect to the host at the given `addr` over SSH.
    ///
    /// The format of `destination` is the same as the `destination` argument to `ssh`. It may be
    /// specified as either `[user@]hostname` or a URI of the form `ssh://[user@]hostname[:port]`.
    ///
    /// If connecting requires interactive authentication based on `STDIN` (such as reading a
    /// password), the connection will fail. Consider setting up keypair-based authentication
    /// instead.
    ///
    /// For more options, see [`SessionBuilder`].
    pub async fn connect<S: AsRef<str>>(destination: S, check: KnownHosts) -> Result<Self, Error> {
        let mut s = SessionBuilder::default();
        s.known_hosts_check(check);
        s.connect(destination.as_ref()).await
    }

    /// Check the status of the underlying SSH connection.
    ///
    /// Since this does not run a remote command, it has a better chance of extracting useful error
    /// messages than other commands.
    pub async fn check(&self) -> Result<(), Error> {
        if self.terminated {
            return Err(Error::Disconnected);
        }

        let check = process::Command::new("ssh")
            .arg("-S")
            .arg(self.ctl_path())
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-O")
            .arg("check")
            .arg(&self.addr)
            .output()
            .await
            .map_err(Error::Ssh)?;

        if let Some(255) = check.status.code() {
            if let Some(master_error) = self.take_master_error().await {
                Err(master_error)
            } else {
                Err(Error::Disconnected)
            }
        } else {
            Ok(())
        }
    }

    /// Constructs a new [`Command`] for launching the program at path `program` on the remote
    /// host.
    ///
    /// Before it is passed to the remote host, `program` is escaped so that special characters
    /// aren't evaluated by the remote shell. If you do not want this behavior, use
    /// [`raw_command`].
    ///
    /// The returned `Command` is a builder, with the following default configuration:
    ///
    /// * No arguments to the program
    /// * Empty stdin and dsicard stdout/stderr for `spawn` or `status`, but create output pipes for
    ///   `output`
    ///
    /// Builder methods are provided to change these defaults and otherwise configure the process.
    ///
    /// If `program` is not an absolute path, the `PATH` will be searched in an OS-defined way on
    /// the host.
    pub fn command<'a, S: Into<Cow<'a, str>>>(&self, program: S) -> Command<'_> {
        self.raw_command(&*shell_escape::unix::escape(program.into()))
    }

    /// Constructs a new [`Command`] for launching the program at path `program` on the remote
    /// host.
    ///
    /// Unlike [`command`], this method does not shell-escape `program`, so it may be evaluated in
    /// unforeseen ways by the remote shell.
    ///
    /// The returned `Command` is a builder, with the following default configuration:
    ///
    /// * No arguments to the program
    /// * Empty stdin and dsicard stdout/stderr for `spawn` or `status`, but create output pipes for
    ///   `output`
    ///
    /// Builder methods are provided to change these defaults and otherwise configure the process.
    ///
    /// If `program` is not an absolute path, the `PATH` will be searched in an OS-defined way on
    /// the host.
    pub fn raw_command<S: AsRef<OsStr>>(&self, program: S) -> Command<'_> {
        // XXX: Should we do a self.check() here first?

        // NOTE: we pass -p 9 nine here (the "discard" port) to ensure that ssh does not
        // succeed in establishing a _new_ connection if the master connection has failed.

        let mut cmd = process::Command::new("ssh");
        cmd.arg("-S")
            .arg(self.ctl_path())
            .arg("-T")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-p")
            .arg("9")
            .arg(&self.addr)
            .arg("--")
            .arg(program);

        Command::new(self, cmd)
    }

    /// Constructs a new [`Command`] that runs the provided shell command on the remote host.
    ///
    /// The provided command is passed as a single, escaped argument to `sh -c`, and from that
    /// point forward the behavior is up to `sh`. Since this executes a shell command, keep in mind
    /// that you are subject to the shell's rules around argument parsing, such as whitespace
    /// splitting, variable expansion, and other funkyness. I _highly_ recommend you read [this
    /// article] if you observe strange things.
    ///
    /// While the returned `Command` is a builder, like for [`command`], you should not add
    /// additional arguments to it, since the arguments are already passed within the shell
    /// command.
    ///
    /// # Non-standard Remote Shells
    ///
    /// It is worth noting that there are really _two_ shells at work here: the one that sshd
    /// launches for the session, and that launches are command; and the instance of `sh` that we
    /// launch _in_ that session. This method tries hard to ensure that the provided `command` is
    /// passed exactly as-is to `sh`, but this is complicated by the presence of the "outer" shell.
    /// That outer shell may itself perform argument splitting, variable expansion, and the like,
    /// which might produce unintuitive results. For example, the outer shell may try to expand a
    /// variable that is only defined in the inner shell, and simply produce an empty string in the
    /// variable's place by the time it gets to `sh`.
    ///
    /// To counter this, this method assumes that the remote shell (the one launched by `sshd`) is
    /// [POSIX compliant]. This is more or less equivalent to "supports `bash` syntax" if you don't
    /// look too closely. It uses [`shell-escape`] to escape `command` before sending it to the
    /// remote shell, with the expectation that the remote shell will only end up undoing that one
    /// "level" of escaping, thus producing the original `command` as an argument to `sh`. This
    /// works _most of the time_.
    ///
    /// With sufficiently complex or weird commands, the escaping of `shell-escape` may not fully
    /// match the "un-escaping" of the remote shell. This will manifest as escape characters
    /// appearing in the `sh` command that you did not intend to be there. If this happens, try
    /// changing the remote shell if you can, or fall back to [`command`] and do the escaping
    /// manually instead.
    ///
    ///   [POSIX compliant]: https://pubs.opengroup.org/onlinepubs/9699919799/xrat/V4_xcu_chap02.html
    ///   [this article]: https://mywiki.wooledge.org/Arguments
    ///   [`shell-escape`]: https://crates.io/crates/shell-escape
    pub fn shell<S: AsRef<str>>(&self, command: S) -> Command<'_> {
        let mut cmd = self.command("sh");
        cmd.arg("-c").arg(command);
        cmd
    }

    /// Prepare to perform file operations on the remote host.
    ///
    /// See [`Sftp`] for details on how to interact with the remote files.
    pub fn sftp(&self) -> Sftp<'_> {
        Sftp::new(self)
    }

    /// Opens a port forwarded to / from the local machine.
    /// Dropping the returned value closes this connection
    pub fn forward_port(
        &self,
        forward_type: PortForwardingType,
        local_addr: ListenAddr,
        remote_addr: ListenAddr,
    ) -> Result<PortForward, Error> {
        PortForward::new(
            self.ctl_path(),
            &self.addr,
            forward_type,
            local_addr,
            remote_addr,
        )
    }

    /// Terminate the remote connection.
    pub async fn close(mut self) -> Result<(), Error> {
        self.terminate().await
    }

    async fn take_master_error(&self) -> Option<Error> {
        let (_stdout, mut stderr) = self.master.lock().unwrap().take()?;

        let mut err = String::new();
        if let Err(e) = stderr.read_to_string(&mut err).await {
            return Some(Error::Master(e));
        }
        let stderr = err.trim();

        if stderr.is_empty() {
            return None;
        }

        let kind = if stderr.contains("Connection to") && stderr.contains("closed by remote host") {
            io::ErrorKind::ConnectionAborted
        } else {
            io::ErrorKind::Other
        };

        Some(Error::Master(io::Error::new(kind, stderr)))
    }

    async fn terminate(&mut self) -> Result<(), Error> {
        if !self.terminated {
            let exit = process::Command::new("ssh")
                .arg("-S")
                .arg(self.ctl_path())
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-O")
                .arg("exit")
                .arg(&self.addr)
                .output()
                .await
                .map_err(Error::Ssh)?;

            self.terminated = true;

            if let Some(master_error) = self.take_master_error().await {
                return Err(master_error);
            }

            if exit.status.success() {
                return Ok(());
            }

            // let's get this case straight:
            // we tried to tell the master to exit.
            // the -O exit command failed.
            // the master exited, but did not produce an error.
            // what could cause that?
            //
            // the only thing I can think of at the moment is that the remote end cleanly
            // closed the connection, probably by virtue of being killed (but without the
            // network dropping out). since we were told to _close_ the connection, well, we
            // have succeeded, so this should not produce an error.
            //
            // we will still _collect_ the error that -O exit produced though,
            // just for ease of debugging.

            let _exit_err = String::from_utf8_lossy(&exit.stderr);
            let _err = _exit_err.trim();
            // eprintln!("{}", _err);
        }

        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.terminated {
            let _ = std::process::Command::new("ssh")
                .arg("-S")
                .arg(self.ctl_path())
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-O")
                .arg("exit")
                .arg(&self.addr)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}
