use openssh::*;
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4},
};
use std::{net::SocketAddr, process::Stdio};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task,
};

// TODO: how do we test the connection actually _failing_ so that the master reports an error?

fn addr() -> String {
    std::env::var("TEST_HOST").unwrap_or("ssh://test-user@127.0.0.1:2222".to_string())
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn it_connects() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();
    session.check().await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn control_dir() {
    let dirname = std::path::Path::new("control-test");
    assert!(!dirname.exists());
    std::fs::create_dir(dirname).unwrap();
    let session = SessionBuilder::default()
        .control_directory(&dirname)
        .connect(&addr())
        .await
        .unwrap();
    session.check().await.unwrap();
    let mut iter = std::fs::read_dir(&dirname).unwrap();
    assert!(iter.next().is_some());
    session.close().await.unwrap();
    std::fs::remove_dir(&dirname).unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn terminate_on_drop() {
    drop(Session::connect(&addr(), KnownHosts::Add).await.unwrap());
    // NOTE: how do we test that it actually killed the master here?
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn stdout() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let child = session.command("echo").arg("foo").output().await.unwrap();
    assert_eq!(child.stdout, b"foo\n");

    let child = session
        .command("echo")
        .arg("foo")
        .raw_arg(">")
        .arg("/dev/stderr")
        .output()
        .await
        .unwrap();
    assert!(child.stdout.is_empty());

    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn shell() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let child = session.shell("echo $USER").output().await.unwrap();
    assert_eq!(child.stdout, b"test-user\n");

    let child = session
        .shell(r#"touch "$USER Documents""#)
        .status()
        .await
        .unwrap();
    assert!(child.success());

    let child = session
        .shell(r#"rm test-user\ Documents"#)
        .status()
        .await
        .unwrap();
    assert!(child.success());

    let child = session.shell("echo \\$SHELL").output().await.unwrap();
    assert_eq!(child.stdout, b"$SHELL\n");

    let child = session
        .shell(r#"echo $USER | grep -c test"#)
        .status()
        .await
        .unwrap();
    assert!(child.success());

    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn stderr() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let child = session.command("echo").arg("foo").output().await.unwrap();
    assert!(child.stderr.is_empty());

    let child = session
        .command("echo")
        .arg("foo")
        .raw_arg(">")
        .arg("/dev/stderr")
        .output()
        .await
        .unwrap();
    assert_eq!(child.stderr, b"foo\n");

    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn stdin() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let mut child = session
        .command("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // write something to standard in and send EOF
    let mut stdin = child.stdin().take().unwrap();
    stdin.write_all(b"hello world").await.unwrap();
    drop(stdin);

    // cat should print it back on stdout
    let mut stdout = child.stdout().take().unwrap();
    let mut out = String::new();
    stdout.read_to_string(&mut out).await.unwrap();
    assert_eq!(out, "hello world");
    drop(stdout);

    // cat should now have terminated
    let status = child.wait().await.unwrap();
    drop(child);

    // ... successfully
    assert!(status.success());

    session.close().await.unwrap();
}

macro_rules! assert_kind {
    ($e:expr, $kind:expr) => {
        let e = $e;
        assert!(
            matches!(e, Error::Remote(ref e) if e.kind() == $kind),
            "{:?}",
            e
        );
    }
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn sftp_can() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let mut sftp = session.sftp();

    // first, do some access checks
    // some things we can do
    sftp.can(Mode::Write, "test_file").await.unwrap();
    sftp.can(Mode::Write, ".ssh/test_file").await.unwrap();
    sftp.can(Mode::Read, ".ssh/authorized_keys").await.unwrap();
    sftp.can(Mode::Read, "/etc/hostname").await.unwrap();
    // some things we cannot
    assert_kind!(
        sftp.can(Mode::Write, "/etc/passwd").await.unwrap_err(),
        io::ErrorKind::PermissionDenied
    );
    assert_kind!(
        sftp.can(Mode::Write, "no/such/file").await.unwrap_err(),
        io::ErrorKind::NotFound
    );
    assert_kind!(
        sftp.can(Mode::Read, "/etc/shadow").await.unwrap_err(),
        io::ErrorKind::PermissionDenied
    );
    assert_kind!(
        sftp.can(Mode::Read, "/etc/no-such-file").await.unwrap_err(),
        io::ErrorKind::NotFound
    );
    assert_kind!(
        sftp.can(Mode::Write, "/etc/no-such-file")
            .await
            .unwrap_err(),
        io::ErrorKind::PermissionDenied
    );
    assert_kind!(
        sftp.can(Mode::Write, "/no-such-file").await.unwrap_err(),
        io::ErrorKind::PermissionDenied
    );
    assert_kind!(
        sftp.can(Mode::Read, "no/such/file").await.unwrap_err(),
        io::ErrorKind::NotFound
    );
    // and something are just weird
    assert_kind!(
        sftp.can(Mode::Write, ".ssh").await.unwrap_err(),
        io::ErrorKind::AlreadyExists
    );
    assert_kind!(
        sftp.can(Mode::Write, "/etc").await.unwrap_err(),
        io::ErrorKind::AlreadyExists
    );
    assert_kind!(
        sftp.can(Mode::Write, "/").await.unwrap_err(),
        io::ErrorKind::AlreadyExists
    );
    assert_kind!(
        sftp.can(Mode::Read, "/etc").await.unwrap_err(),
        io::ErrorKind::Other
    );

    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn sftp() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let mut sftp = session.sftp();

    // first, open a file for writing
    let mut w = sftp.write_to("test_file").await.unwrap();

    // reading from a write-only file should error
    let failed = w.read(&mut [0]).await.unwrap_err();
    assert_eq!(failed.kind(), io::ErrorKind::UnexpectedEof);

    // write something to the file
    w.write_all(b"hello").await.unwrap();
    w.close().await.unwrap();

    // we should still be able to write it
    sftp.can(Mode::Write, "test_file").await.unwrap();
    // and now also read it
    sftp.can(Mode::Read, "test_file").await.unwrap();

    // open the file again for appending
    let mut w = sftp.append_to("test_file").await.unwrap();

    // reading from an append-only file should also error
    let failed = w.read(&mut [0]).await.unwrap_err();
    assert_eq!(failed.kind(), io::ErrorKind::UnexpectedEof);

    // append something to the file
    w.write_all(b" world").await.unwrap();
    w.close().await.unwrap();

    // then, open the same file for reading
    let mut r = sftp.read_from("test_file").await.unwrap();

    // writing to a read-only file should error
    let failed = r.write(&[0]).await.unwrap_err();
    assert_eq!(failed.kind(), io::ErrorKind::WriteZero);

    // read back the file
    let mut contents = String::new();
    r.read_to_string(&mut contents).await.unwrap();
    assert_eq!(contents, "hello world");
    r.close().await.unwrap();

    // reading a file that does not exist should error on open
    let failed = sftp.read_from("no/such/file").await.unwrap_err();
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::NotFound));
    // so should file we're not allowed to read
    let failed = sftp.read_from("/etc/shadow").await.unwrap_err();
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::PermissionDenied));

    // writing a file that does not exist should also error on open
    let failed = sftp.write_to("no/such/file").await.unwrap_err();
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::NotFound));
    // so should file we're not allowed to write
    let failed = sftp.write_to("/rootfile").await.unwrap_err();
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::PermissionDenied));

    // writing to a full disk (or the like) should also error
    let mut w = sftp.write_to("/dev/full").await.unwrap();
    w.write_all(b"hello world").await.unwrap();
    let failed = w.close().await.unwrap_err();
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::WriteZero));

    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn bad_remote_command() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    // a bad remote command should result in a _local_ error.
    let failed = session
        .command("no such program")
        .output()
        .await
        .unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::NotFound));

    // no matter how you run it
    let failed = session
        .command("no such program")
        .status()
        .await
        .unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::NotFound));

    // even if you spawn first
    let mut child = session.command("no such program").spawn().unwrap();
    let failed = child.wait().await.unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::NotFound));
    child.disconnect().await.unwrap_err();

    // of if you want output
    let child = session.command("no such program").spawn().unwrap();
    let failed = child.wait_with_output().await.unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::NotFound));

    // no matter how hard you _try_
    let mut child = session.command("no such program").spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let failed = child.try_wait().unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Remote(ref e) if e.kind() == io::ErrorKind::NotFound));
    child.disconnect().await.unwrap_err();

    session.close().await.unwrap();
}

#[tokio::test]
async fn connect_timeout() {
    use std::time::{Duration, Instant};

    let t = Instant::now();
    let mut sb = SessionBuilder::default();
    let failed = sb
        .connect_timeout(Duration::from_secs(1))
        .connect("192.0.0.8")
        .await
        .unwrap_err();
    assert!(t.elapsed() > Duration::from_secs(1));
    assert!(t.elapsed() < Duration::from_secs(2));
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Connect(ref e) if e.kind() == io::ErrorKind::TimedOut));
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn spawn_and_wait() {
    use std::time::{Duration, Instant};

    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let t = Instant::now();
    let sleeping1 = session.command("sleep").arg("1").spawn().unwrap();
    let sleeping2 = sleeping1
        .session()
        .command("sleep")
        .arg("2")
        .spawn()
        .unwrap();
    sleeping1.wait_with_output().await.unwrap();
    assert!(t.elapsed() > Duration::from_secs(1));
    sleeping2.wait_with_output().await.unwrap();
    assert!(t.elapsed() > Duration::from_secs(2));

    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn escaping() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let status = dbg!(session
        .command("printf")
        .arg("%d %d")
        .arg("1")
        .arg("2")
        .output()
        .await
        .unwrap())
    .status;
    assert!(status.success());

    let status = dbg!(session
        .command("printf")
        .args(vec!["%d %d", "1", "2"])
        .output()
        .await
        .unwrap())
    .status;
    assert!(status.success());

    let status = dbg!(session
        .command("printf")
        .arg("%d %d")
        .raw_arg("1 2")
        .output()
        .await
        .unwrap())
    .status;
    assert!(status.success());

    let status = dbg!(session
        .command("printf")
        .arg("%d %d")
        .raw_args(std::iter::once("1 2"))
        .output()
        .await
        .unwrap())
    .status;
    assert!(status.success());

    let status = dbg!(session
        .raw_command("printf '%d %d'")
        .arg("1")
        .arg("2")
        .output()
        .await
        .unwrap())
    .status;
    assert!(status.success());

    session.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn process_exit_on_signal() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let mut sleeping = session.command("sleep").arg("5566").spawn().unwrap();

    // give it some time to make sure it starts
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Now stop that process.
    //
    // We use `pkill -f` to match on the number rather than the `sleep` command, since other tests
    // may use `sleep`. We use `-o` to ensure that we don't accidentally kill the ssh connection
    // itself, but instead match the _oldest_ matching command.
    let killed = session
        .command("pkill")
        .arg("-f")
        .arg("-o")
        .arg("5566")
        .output()
        .await
        .unwrap();
    eprintln!("{:?}", killed);
    assert!(killed.status.success());

    // await that process — this will yield "Disconnected", since the remote process disappeared
    let failed = sleeping.wait().await.unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Disconnected));

    // the connection should still work though
    let _ = session.check().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(ci), ignore)]
async fn broken_connection() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();

    let sleeping = session.command("sleep").arg("1000").spawn().unwrap();

    // get ID of remote ssh process
    let ppid = session
        .command("echo")
        .raw_arg("$PPID")
        .output()
        .await
        .unwrap();
    eprintln!("ppid: {:?}", ppid);
    let ppid: u32 = String::from_utf8(ppid.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // and kill it -- this kills the master connection
    let killed = session
        .command("kill")
        .arg("-9")
        .arg(&format!("{}", ppid))
        .output()
        .await
        .unwrap_err();
    eprintln!("{:?}", killed);
    assert!(matches!(killed, Error::Disconnected));

    // this fails because the master connection is gone
    let failed = session
        .command("echo")
        .arg("foo")
        .output()
        .await
        .unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Disconnected));

    // so does this
    let failed = session
        .command("echo")
        .arg("foo")
        .status()
        .await
        .unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Disconnected));

    // the spawned child we're waiting for must also have failed
    let failed = sleeping.wait_with_output().await.unwrap_err();
    eprintln!("{:?}", failed);
    assert!(matches!(failed, Error::Disconnected));

    // check should obviously fail
    let failed = session.check().await.unwrap_err();
    if let Error::Master(ref ioe) = failed {
        if ioe.kind() != io::ErrorKind::ConnectionAborted {
            eprintln!("{:?}", ioe);
            assert_eq!(ioe.kind(), io::ErrorKind::ConnectionAborted);
        }
    } else {
        unreachable!("{:?}", failed);
    }

    // what should close do in this instance?
    // probably not return an error, since the connection _is_ closed.
    session.close().await.unwrap();
}

#[tokio::test]
async fn cannot_resolve() {
    match Session::connect("bad-host", KnownHosts::Accept)
        .await
        .unwrap_err()
    {
        Error::Connect(e) => {
            eprintln!("{:?}", e);
            assert_eq!(e.kind(), io::ErrorKind::Other);
        }
        e => unreachable!("{:?}", e),
    }
}

#[tokio::test]
async fn no_route() {
    match Session::connect("255.255.255.255", KnownHosts::Accept)
        .await
        .unwrap_err()
    {
        Error::Connect(e) => {
            eprintln!("{:?}", e);
            assert_eq!(e.kind(), io::ErrorKind::Other);
        }
        e => unreachable!("{:?}", e),
    }
}

#[tokio::test]
async fn connection_refused() {
    match Session::connect("ssh://127.0.0.1:9", KnownHosts::Accept)
        .await
        .unwrap_err()
    {
        Error::Connect(e) => {
            eprintln!("{:?}", e);
            assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
        }
        e => unreachable!("{:?}", e),
    }
}

#[tokio::test]
async fn auth_failed() {
    let addr = if cfg!(ci) {
        // prefer the known-accessible test server when available
        addr().replace("test-user", "bad-user")
    } else {
        String::from("ssh://openssh-tester@login.csail.mit.edu")
    };

    match Session::connect(&addr, KnownHosts::Accept)
        .await
        .unwrap_err()
    {
        Error::Connect(e) => {
            eprintln!("{:?}", e);
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        }
        e => unreachable!("{:?}", e),
    }
}

#[tokio::test]
async fn forward_local_port_to_remote() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();
    session.check().await.unwrap();
    let listen_task = task::spawn(async {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 2020))
            .await
            .unwrap();
        let (mut client, _) = listener.accept().await.unwrap();
        let mut buffer = String::new();
        client.read_to_string(&mut buffer).await.unwrap();
        assert_eq!(buffer, "hello world");
    });

    let forward = session
        .forward_port(
            PortForwardingType::LocalPortToRemote,
            ListenAddr::SocketAddr(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                2020,
            ))),
            ListenAddr::SocketAddr(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                2050,
            ))),
        )
        .unwrap();
    let mut client = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 2050))
        .await
        .unwrap();
    client.write_all(b"hello world").await.unwrap();
    client.flush().await.unwrap();
    drop(client);

    listen_task.await.unwrap();
    drop(forward);

    session.close().await.unwrap();
}

#[tokio::test]
async fn forward_remote_port_to_local() {
    let session = Session::connect(&addr(), KnownHosts::Accept).await.unwrap();
    session.check().await.unwrap();
    let listen_task = task::spawn(async {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 2020))
            .await
            .unwrap();
        let (mut client, _) = listener.accept().await.unwrap();
        let mut buffer = String::new();
        client.read_to_string(&mut buffer).await.unwrap();
        assert_eq!(buffer, "hello world");
    });

    let forward = session
        .forward_port(
            PortForwardingType::RemotePortToLocal,
            ListenAddr::SocketAddr(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                2020,
            ))),
            ListenAddr::SocketAddr(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, 1),
                2050,
            ))),
        )
        .unwrap();
    let mut client = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 2050))
        .await
        .unwrap();
    client.write_all(b"hello world").await.unwrap();
    client.flush().await.unwrap();
    drop(client);

    listen_task.await.unwrap();
    drop(forward);

    session.close().await.unwrap();
}
