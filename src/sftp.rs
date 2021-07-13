use super::{Error, Session};
use std::io;
use std::process::Stdio;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::AsyncWriteExt;

// TODO: it would _probably_ be better to actually use sftp here, since I'm pretty sure it has some
// kind of protocol optimized for binary data. but for now, this is fine.

/// A file-oriented channel to a remote host.
///
/// You likely want [`Sftp::write_to`] and [`Sftp::read_from`].
#[derive(Debug, Clone)]
pub struct Sftp<'s> {
    session: &'s Session,
}

/// A file access mode.
#[derive(Debug, Clone, Copy)]
pub enum Mode {
    /// Read-only access.
    Read,
    /// Write-only access.
    Write,
    /// Write-only access in append mode.
    Append,
}

impl Mode {
    fn is_write(self) -> bool {
        match self {
            Mode::Append | Mode::Write => true,
            Mode::Read => false,
        }
    }
}

/// A handle to a remote file.
///
/// If you opened this file for reading (with [`Sftp::read_from`]), you can read from it with
/// [`std::io::Read`]. If you opened it for writing (with [`Sftp::write_to`]), you can write to it
/// with [`std::io::Write`].
///
/// Note that because we are operating against a remote host, errors may take a while to propagate.
/// The various methods on `RemoteFile` will generally attempt to first check that the file you are
/// trying to access can indeed be accessed in that way, but some errors may not become visible
/// until you call [`close`](RemoteFile::close).  In particular, the connection between you and the
/// remote host may buffer bytes, so your write may report that some number of bytes have been
/// successfully written, even though the remote disk is full. Or the file you are reading from may
/// have been removed between when [`read_from`](Sftp::read_from) checks that it exists and when it
/// actually tries to read the first byte. For that reason, you should **make sure to call
/// [`close`](RemoteFile::close)** to observe any errors that may have occurred when operating on
/// the remote file.
#[derive(Debug)]
pub struct RemoteFile<'s> {
    cat: super::RemoteChild<'s>,
    mode: Mode,
}

// TODO: something like std::fs::OpenOptions

impl<'s> Sftp<'s> {
    pub(crate) fn new(session: &'s Session) -> Self {
        Self { session }
    }

    // TODO: remove
    // TODO: exists

    /// Check that the given file can be opened in the given mode.
    ///
    /// This method does not change the remote file system, except where opening a file for
    /// read/write alone causes changes (like the "last accessed" timestamp).
    ///
    /// Note that this function is potentially racy. The permissions on the server could change
    /// between when you check and when you start a subsequent operation, causing it to fail. It
    /// can also produce false positives, such as if you are checking whether you can write to a
    /// file on a read-only file system that you still have write permissions on.
    ///
    /// Operations like [`read_from`](Sftp::read_from) and [`write_to`](Sftp::write_to) internally
    /// perform similar checking to this method, so you do not need to call `can` before calling
    /// those methods. The checking performed by `write_to` can also test its permissions by
    /// actually attemping to create the remote file (since it is about to create one anyway), so
    /// its checking is more reliable than what `can` can provide.
    pub async fn can(
        &mut self,
        mode: Mode,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), Error> {
        let path = path.as_ref();

        // okay, so, I know it's weird to use dd for this, but hear me out here.
        // we need a command that:
        //
        //  - will error if you try to read from or write to a directory
        //  - will error if you try to read from or write to a file you do not have access to
        //  - disinguishes between the two former cases
        //  - does not create the file if it does not exist
        //  - will succeed otherwise
        //
        // cat would work for read, but would read the entire file (no good)
        // stat would not tell us if we can actually read or write the file
        // touch won't work, as it also works on directories (no error)
        // ls won't work unless we want to parse its output (which we don't)
        // test won't work because it doesn't tell us _which_ test failed
        //
        // dd does everything we need!
        let mut cmd = self.session.command("dd");
        if mode.is_write() {
            cmd.arg("if=/dev/null")
                .arg(&format!("of={}", path.display()))
                .arg("bs=1")
                .arg("count=0")
                .arg("conv=nocreat,notrunc");
        } else {
            cmd.arg(&format!("if={}", path.display()))
                .arg("of=/dev/null")
                .arg("bs=1")
                .arg("count=1");
        }

        let test = cmd.output().await?;
        if test.status.success() {
            return Ok(());
        }

        // we _could_ fail here because the file does not exist.
        // if this is a write, we still need to check that the parent dir is writeable.

        // let's find out _why_ it failed
        let stderr = String::from_utf8_lossy(&test.stderr);
        let stderr = stderr.trim().trim_start_matches("dd: ");
        if stderr.contains("No such file or directory") {
            if !mode.is_write() {
                // a file that does not exist cannot be read
                return Err(Error::Remote(io::Error::new(
                    io::ErrorKind::NotFound,
                    &*stderr,
                )));
            } else {
                // fall-through
            }
        } else if stderr.contains("Is a directory") {
            if mode.is_write() {
                return Err(Error::Remote(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    &*stderr,
                )));
            } else {
                return Err(Error::Remote(io::Error::new(
                    io::ErrorKind::Other,
                    &*stderr,
                )));
            }
        } else {
            return Err(Error::Remote(io::Error::new(
                io::ErrorKind::PermissionDenied,
                &*stderr,
            )));
        }

        // file does not exist, but a write may be able to create it
        // we need to check if the target parent is a writeable dir
        let dir = if let Some(dir) = path.parent() {
            if dir == std::path::Path::new("") {
                std::path::Path::new(".")
            } else {
                dir
            }
        } else {
            // they're trying to write to / itself?
            return Err(Error::Remote(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "trying to write to root as a file",
            )));
        };

        // https://github.com/sfackler/shell-escape/issues/5
        let dir = dir.to_string_lossy();

        let test = self
            .session
            .command("test")
            .arg("(")
            .arg("-d")
            .arg(&dir)
            .arg(")")
            .arg("-a")
            .arg("(")
            .arg("-w")
            .arg(&dir)
            .arg(")")
            .output()
            .await?;

        if test.status.success() {
            Ok(())
        } else if self
            .session
            .command("test")
            .arg("-e")
            .arg(&dir)
            .status()
            .await?
            .success()
        {
            Err(Error::Remote(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "parent directory not writeable",
            )))
        } else {
            Err(Error::Remote(io::Error::new(
                io::ErrorKind::NotFound,
                "parent directory does not exist",
            )))
        }
    }

    /// Initialize the given operation on the target file.
    ///
    /// Note that this function is not guaranteed to be side-effect free for writes. Specifically,
    /// it will create the target file in order to test whether it is writeable.
    async fn init_op(
        &mut self,
        mode: Mode,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), Error> {
        if mode.is_write() {
            // https://github.com/sfackler/shell-escape/issues/5
            let path = path.as_ref().to_string_lossy();

            // for writes, we want a stronger (and cheaper) test than can()
            // we can't actually use `touch`, since it also works for dirs
            // note that this works b/c stdin is Stdio::null()
            let touch = self
                .session
                .command("cat")
                .raw_arg(">>")
                .arg(&path)
                .stdin(Stdio::null())
                .output()
                .await?;
            if touch.status.success() {
                return Ok(());
            }

            // let's find out _why_ it failed
            let stderr = String::from_utf8_lossy(&touch.stderr);
            if stderr.contains("No such file or directory") {
                Err(Error::Remote(io::Error::new(
                    io::ErrorKind::NotFound,
                    &*stderr,
                )))
            } else {
                Err(Error::Remote(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    &*stderr,
                )))
            }
        } else {
            self.can(Mode::Read, path).await
        }
    }

    /// Open the remote file at `path` for writing.
    ///
    /// If the remote file exists, it will be truncated. If it does not, it will be created.
    ///
    /// Note that some errors may not propagate until you call [`close`](RemoteFile::close). This
    /// method internally performs similar checks to [`can`](Sftp::can) though, so you should not
    /// need to call `can` before calling this method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use openssh::*;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use std::io::prelude::*;
    ///
    /// // connect to a remote host and get an sftp connection
    /// let session = Session::connect("host", KnownHosts::Strict).await?;
    /// let mut sftp = session.sftp();
    ///
    /// // open a file for writing
    /// let mut w = sftp.write_to("test_file").await?;
    ///
    /// // write something to the file
    /// use tokio::io::AsyncWriteExt;
    /// w.write_all(b"hello world").await?;
    ///
    /// // flush and close the remote file, absorbing any final errors
    /// w.close().await?;
    /// # Ok(())
    /// # }
    pub async fn write_to(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<RemoteFile<'s>, Error> {
        let path = path.as_ref();

        self.init_op(Mode::Write, path).await?;

        // https://github.com/sfackler/shell-escape/issues/5
        let path = path.to_string_lossy();

        let cat = self
            .session
            .command("tee")
            .arg(path)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Ok(RemoteFile {
            cat,
            mode: Mode::Write,
        })
    }

    /// Open the remote file at `path` for appending.
    ///
    /// If the remote file exists, it will be appended to. If it does not, it will be created.
    ///
    /// Note that some errors may not propagate until you call [`close`](RemoteFile::close). This
    /// method internally performs similar checks to [`can`](Sftp::can) though, so you should not
    /// need to call `can` before calling this method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use openssh::*;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use std::io::prelude::*;
    ///
    /// // connect to a remote host and get an sftp connection
    /// let session = Session::connect("host", KnownHosts::Strict).await?;
    /// let mut sftp = session.sftp();
    ///
    /// // open a file for appending
    /// let mut w = sftp.append_to("test_file").await?;
    ///
    /// // write will append to the file
    /// use tokio::io::AsyncWriteExt;
    /// w.write_all(b"hello world").await?;
    ///
    /// // flush and close the remote file, absorbing any final errors
    /// w.close().await?;
    /// # Ok(())
    /// # }
    pub async fn append_to(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<RemoteFile<'s>, Error> {
        let path = path.as_ref();

        self.init_op(Mode::Append, path).await?;

        // https://github.com/sfackler/shell-escape/issues/5
        let path = path.to_string_lossy();

        let cat = self
            .session
            .command("tee")
            .arg("-a")
            .arg(path)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Ok(RemoteFile {
            cat,
            mode: Mode::Append,
        })
    }

    /// Open the remote file at `path` for reading.
    ///
    /// Note that some errors may not propagate until you call [`close`](RemoteFile::close). This
    /// method internally performs similar checks to [`can`](Sftp::can) though, so you should not
    /// need to call `can` before calling this method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use openssh::*;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use std::io::prelude::*;
    ///
    /// // connect to a remote host and get an sftp connection
    /// let session = Session::connect("host", KnownHosts::Strict).await?;
    /// let mut sftp = session.sftp();
    ///
    /// // open a file for reading
    /// let mut r = sftp.read_from("/etc/hostname").await?;
    ///
    /// // write something to the file
    /// use tokio::io::AsyncReadExt;
    /// let mut contents = String::new();
    /// r.read_to_string(&mut contents).await?;
    ///
    /// // close the remote file, absorbing any final errors
    /// r.close().await?;
    /// # Ok(())
    /// # }
    pub async fn read_from(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<RemoteFile<'s>, Error> {
        let path = path.as_ref();

        self.init_op(Mode::Read, path).await?;

        // https://github.com/sfackler/shell-escape/issues/5
        let path = path.to_string_lossy();

        let cat = self
            .session
            .command("cat")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Ok(RemoteFile {
            cat,
            mode: Mode::Read,
        })
    }
}

impl RemoteFile<'_> {
    /// Close the handle to the remote file.
    ///
    /// If the remote file was opened for reading, this will also call
    /// [`flush`](AsyncWriteExt::flush).
    ///
    /// When you close the remote file, any errors on the remote end will also be propagated. This
    /// means that you could see errors about remote files not existing, or disks being full, only
    /// at the time when you call `close`.
    pub async fn close(mut self) -> Result<(), Error> {
        if self.mode.is_write() {
            self.flush().await.map_err(Error::Remote)?;
        }

        let mut result = self.cat.wait_with_output().await?;
        if result.status.success() {
            return Ok(());
        }

        // let us try to cobble together a good error for the user
        let err = match String::from_utf8(std::mem::take(&mut result.stderr)) {
            Err(e) => {
                return Err(Error::Remote(io::Error::new(io::ErrorKind::Other, e)));
            }
            Ok(s) => s,
        };
        let err = err.trim();

        if let Some(1) = result.status.code() {
            // looking at cat's source at the time of writing:
            // https://github.com/coreutils/coreutils/blob/730876d067f24380ccec1bdd1f179a664f11aa2f/src/cat.c
            // cat always returns with EXIT_FAILURE, which is 1 on all POSIX platforms.
        } else {
            // something really weird happened.
            return Err(Error::Remote(io::Error::new(io::ErrorKind::Other, err)));
        }

        // search for "die" or EXIT_FAILURE in the cat source code:
        // https://github.com/coreutils/coreutils/blob/master/src/cat.c
        #[allow(clippy::collapsible_if)]
        Err(Error::Remote(if self.mode.is_write() {
            if err.ends_with(": No such file or directory") {
                io::Error::new(io::ErrorKind::NotFound, err)
            } else {
                io::Error::new(io::ErrorKind::WriteZero, err)
            }
        } else if err.ends_with(": No such file or directory") {
            io::Error::new(io::ErrorKind::NotFound, err)
        } else {
            io::Error::new(io::ErrorKind::UnexpectedEof, err)
        }))
    }
}

impl tokio::io::AsyncRead for RemoteFile<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.mode.is_write() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "attempted to read from remote file opened for writing",
            )));
        }

        Pin::new(self.cat.stdout().as_mut().unwrap()).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for RemoteFile<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if !self.mode.is_write() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "attempted to write to remote file opened for reading",
            )));
        }

        Pin::new(self.cat.stdin().as_mut().unwrap()).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(self.cat.stdin().as_mut().unwrap()).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(self.cat.stdin().as_mut().unwrap()).poll_shutdown(cx)
    }
}
