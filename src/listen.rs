//! `sshound listen`: accept players and play what they send.

use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
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
/// Volume in percent until one has been set.
const DEFAULT_VOLUME: u8 = 100;

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
    /// Where the volume is kept between players; none when discarding audio.
    volume_file: Option<PathBuf>,
}

/// Run the listener in the foreground until killed.
pub fn run(opts: Opts) -> Result<()> {
    let addrs = if opts.bind.is_empty() {
        let tailnet = crate::tailnet::addrs();
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
    Output::open(Arc::new(Mutex::new(Source::new())), opts.null)
        .context("cannot open audio output")?;

    let server = Server {
        name: crate::hostname(),
        null: opts.null,
        log: true,
        players: Arc::new(AtomicUsize::new(0)),
        volume_file: volume_file().filter(|_| !opts.null),
    };
    let mut threads = Vec::new();
    for ip in addrs {
        let addr = SocketAddr::new(ip, opts.port);
        let listener =
            TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
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
    // The same volume as `sshound listen` keeps, so that playing here sounds
    // the same as playing from elsewhere.
    spawn_private(null, volume_file().filter(|_| !null))
}

/// A private listener that discards the audio but keeps its volume in `file`.
#[cfg(test)]
pub fn spawn_null_saving(file: PathBuf) -> Result<SocketAddr> {
    spawn_private(true, Some(file))
}

fn spawn_private(null: bool, volume_file: Option<PathBuf>) -> Result<SocketAddr> {
    Output::open(Arc::new(Mutex::new(Source::new())), null)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let addr = listener.local_addr()?;
    let server = Server {
        name: "this machine".into(),
        null,
        log: false,
        players: Arc::new(AtomicUsize::new(0)),
        volume_file,
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
                let peer = stream
                    .peer_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_default();
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
    let peer = match proto::read_hello(&mut stream) {
        Ok(peer) => peer,
        // Gone without a word: a player checking that we are here.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let source: Shared = Arc::new(Mutex::new(Source::new()));
    // Read afresh for every player: another process may have changed it.
    source
        .lock()
        .expect("audio source lock")
        .set_volume(load_volume(server.volume_file.as_deref()));
    let output = Output::open(source.clone(), server.null).context("cannot open audio output")?;
    proto::write_hello(&mut stream, &server.name)?;
    if server.log {
        eprintln!("+ {peer}");
    }

    let done = Arc::new(AtomicBool::new(false));
    let keeper = {
        let (stream, source, done, null) = (
            stream.try_clone()?,
            source.clone(),
            done.clone(),
            server.null,
        );
        thread::spawn(move || keep(stream, source, output, null, done))
    };
    let result = receive(&mut stream, &source, server);
    done.store(true, Ordering::Relaxed);
    let _ = keeper.join();
    if server.log {
        eprintln!("- {peer}");
    }
    result
}

/// Apply the player's commands until it goes away.
fn receive(stream: &mut TcpStream, source: &Shared, server: &Server) -> Result<()> {
    let lock = || source.lock().expect("audio source lock");
    let mut save_failed = false;
    loop {
        let cmd = match Cmd::read(stream) {
            Ok(cmd) => cmd,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
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
            Cmd::Volume(v) => {
                lock().set_volume(v);
                if let Some(path) = &server.volume_file
                    && let Err(e) = save_volume(path, v)
                    && server.log
                    && !save_failed
                {
                    save_failed = true;
                    eprintln!("cannot save the volume in {}: {e}", path.display());
                }
            }
            Cmd::Ping => {}
        }
    }
}

/// Report the playhead back to the player and keep the output device in step
/// with what is wanted: running while there is something to play, stopped
/// after a moment of pause, reopened if it breaks.
fn keep(
    mut stream: TcpStream,
    source: Shared,
    mut output: Output,
    null: bool,
    done: Arc<AtomicBool>,
) {
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

/// Where the volume is kept, for `sshound listen` and for playing on this
/// machine alike.
fn volume_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute());
    let dir = if cfg!(target_os = "macos") {
        home?.join("Library/Application Support")
    } else {
        match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() => dir,
            _ => home?.join(".local/state"),
        }
    };
    Some(dir.join("sshound/volume"))
}

fn load_volume(path: Option<&Path>) -> u8 {
    path.and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse::<u8>().ok())
        .map_or(DEFAULT_VOLUME, |percent| percent.min(100))
}

/// Replace the file in one step, so that a reader never sees half of it.
fn save_volume(path: &Path, percent: u8) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension(format!("{}.partial", std::process::id()));
    fs::write(&partial, format!("{percent}\n"))?;
    fs::rename(&partial, path).inspect_err(|_| {
        let _ = fs::remove_file(&partial);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_survives_in_a_file() {
        let dir = std::env::temp_dir().join(format!("sshound-volume-{}", std::process::id()));
        let path = dir.join("state/sshound/volume");
        assert_eq!(
            load_volume(Some(&path)),
            DEFAULT_VOLUME,
            "nothing saved yet"
        );
        assert_eq!(load_volume(None), DEFAULT_VOLUME);

        save_volume(&path, 35).unwrap();
        assert_eq!(load_volume(Some(&path)), 35);
        save_volume(&path, 80).unwrap();
        assert_eq!(load_volume(Some(&path)), 80);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        fs::write(&path, "loud\n").unwrap();
        assert_eq!(load_volume(Some(&path)), DEFAULT_VOLUME);
        fs::write(&path, "-3").unwrap();
        assert_eq!(load_volume(Some(&path)), DEFAULT_VOLUME);
        fs::remove_dir_all(&dir).unwrap();
    }
}
