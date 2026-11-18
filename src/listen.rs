//! `sshound listen`: accept players and play what they send.

use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::output::{Output, Shared};
use crate::proto::{self, Cmd, Report};
use crate::source::{MAX_QUEUE, Source};

const MAX_PLAYERS: usize = 8;
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Opts {
    pub port: u16,
    pub bind: Vec<IpAddr>,
    pub null: bool,
}

#[derive(Clone)]
struct Server {
    name: String,
    null: bool,
    log: bool,
    players: Arc<AtomicUsize>,
}

/// Run the listener in the foreground until killed.
pub fn run(opts: Opts) -> Result<()> {
    let addrs = if opts.bind.is_empty() {
        let tailnet = tailscale_addrs();
        if tailnet.is_empty() {
            eprintln!(
                "no Tailscale address found: listening on localhost only \
                 (reach it with `ssh -R {0}:localhost:{0} …`, or pass --bind)",
                opts.port
            );
        }
        let mut addrs = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
        addrs.extend(tailnet);
        addrs
    } else {
        opts.bind
    };

    // Fail now, not on the first connection, if there is nothing to play through.
    Output::open(Arc::new(Mutex::new(Source::new())), opts.null).context("cannot open audio output")?;

    let server = Server {
        name: crate::hostname(),
        null: opts.null,
        log: true,
        players: Arc::new(AtomicUsize::new(0)),
    };
    let mut threads = Vec::new();
    for ip in addrs {
        let addr = SocketAddr::new(ip, opts.port);
        let listener = TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
        eprintln!("listening on {addr}");
        let server = server.clone();
        threads.push(thread::spawn(move || accept(listener, server)));
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

/// Start a private listener on localhost for playing on this machine.
pub fn spawn_local(null: bool) -> Result<SocketAddr> {
    Output::open(Arc::new(Mutex::new(Source::new())), null)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let addr = listener.local_addr()?;
    let server = Server {
        name: "this machine".into(),
        null,
        log: false,
        players: Arc::new(AtomicUsize::new(0)),
    };
    thread::spawn(move || accept(listener, server));
    Ok(addr)
}

fn accept(listener: TcpListener, server: Server) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        let server = server.clone();
        thread::spawn(move || {
            if server.players.fetch_add(1, Ordering::SeqCst) < MAX_PLAYERS {
                let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
                if let Err(e) = serve(stream, &server)
                    && server.log
                {
                    eprintln!("{peer}: {e:#}");
                }
            }
            server.players.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn serve(mut stream: TcpStream, server: &Server) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let peer = proto::read_hello(&mut stream)?;

    let source: Shared = Arc::new(Mutex::new(Source::new()));
    let output = Output::open(source.clone(), server.null).context("cannot open audio output")?;
    proto::write_hello(&mut stream, &server.name)?;
    if server.log {
        eprintln!("+ {peer}");
    }

    let done = Arc::new(AtomicBool::new(false));
    let keeper = {
        let (stream, source, done, null) = (stream.try_clone()?, source.clone(), done.clone(), server.null);
        thread::spawn(move || keep(stream, source, output, null, done))
    };
    let result = receive(&mut stream, &source);
    done.store(true, Ordering::Relaxed);
    let _ = keeper.join();
    if server.log {
        eprintln!("- {peer}");
    }
    result
}

/// Apply the player's commands until it goes away.
fn receive(stream: &mut TcpStream, source: &Shared) -> Result<()> {
    let lock = || source.lock().expect("audio source lock");
    loop {
        let cmd = match Cmd::read(stream) {
            Ok(cmd) => cmd,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                anyhow::bail!("timed out")
            }
            Err(e) => return Err(e.into()),
        };
        match cmd {
            Cmd::Audio(pcm) => {
                // A well-behaved player never gets close to this; just stop reading until
                // there is room.
                let waiting = Instant::now();
                while lock().queued() > MAX_QUEUE {
                    anyhow::ensure!(waiting.elapsed() < Duration::from_secs(60), "queue stuck");
                    thread::sleep(Duration::from_millis(5));
                }
                lock().push(&pcm);
            }
            Cmd::Flush(epoch) => lock().flush(epoch),
            Cmd::Pause(on) => lock().set_paused(on),
            Cmd::Volume(v) => lock().set_volume(v),
            Cmd::Ping => {}
        }
    }
}

/// Report the playhead back to the player and keep the output device in step
/// with what is wanted: running while there is something to play, stopped
/// after a moment of pause, reopened if it breaks.
fn keep(mut stream: TcpStream, source: Shared, mut output: Output, null: bool, done: Arc<AtomicBool>) {
    let mut last = Report::default();
    let mut last_sent: Option<Instant> = None;
    let mut idle_since: Option<Instant> = None;
    let mut last_reopen = Instant::now();

    while !done.load(Ordering::Relaxed) {
        let (report, wanted) = match source.lock() {
            Ok(s) => (s.report(), s.wants_output()),
            Err(_) => break,
        };

        if !output.healthy() && last_reopen.elapsed() > Duration::from_secs(1) {
            last_reopen = Instant::now();
            if let Ok(fresh) = Output::open(source.clone(), null) {
                output = fresh;
            }
        }
        if wanted {
            idle_since = None;
            output.set_running(true);
        } else if idle_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(1) {
            output.set_running(false);
        }

        if report != last || last_sent.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
            if stream.write_all(&report.to_bytes()).is_err() {
                break;
            }
            last = report;
            last_sent = Some(Instant::now());
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Unblocks `receive` if we are the one giving up.
    let _ = stream.shutdown(Shutdown::Both);
}

/// This machine's Tailscale addresses, found by asking the OS which local
/// address it would use to reach Tailscale's own service address. No packets
/// are sent.
fn tailscale_addrs() -> Vec<IpAddr> {
    let probe = |bind: &str, to: &str| -> Option<IpAddr> {
        let socket = UdpSocket::bind(bind).ok()?;
        socket.connect(to).ok()?;
        Some(socket.local_addr().ok()?.ip())
    };
    let v4 = probe("0.0.0.0:0", "100.100.100.100:53").filter(|ip| match ip {
        // 100.64.0.0/10
        IpAddr::V4(a) => a.octets()[0] == 100 && a.octets()[1] & 0xc0 == 64,
        _ => false,
    });
    let v6 = probe("[::]:0", "[fd7a:115c:a1e0::53]:53").filter(|ip| match ip {
        // fd7a:115c:a1e0::/48
        IpAddr::V6(a) => a.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
        _ => false,
    });
    v4.into_iter().chain(v6).collect()
}
