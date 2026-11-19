//! The playback engine on the remote side: holds the decoded track, keeps a
//! connection to the listener, and feeds it audio a little ahead of the playhead.
//!
//! The listener's audio device is the clock. It reports how much it has taken
//! and how much has been heard; we top its queue up to `WINDOW` and show the
//! heard position. Pause, volume and seek are commands, so they act at once
//! however much audio is queued.
//!
//! The volume belongs to the listener, which remembers it: we show what it
//! reports and ask it for changes.

use std::io::{self, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::proto::{self, Cmd, FRAME, RATE, Report};

/// How much audio to keep queued at the listener.
const WINDOW: u64 = RATE as u64 / 2;
/// Frames per audio message (20 ms).
const CHUNK: u64 = RATE as u64 / 50;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
/// The listener reports at least twice a second; this long without one means it is gone.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    /// Not connected; trying this address.
    Waiting(String),
    /// Something at this address took the connection and hung up without a
    /// greeting, as a listener running another version of sshound does.
    HungUp(String),
    /// Connected to the named machine.
    Connected(String),
}

#[derive(Clone, Debug)]
pub struct Status {
    /// Playhead, in frames.
    pub pos: u64,
    /// Whether playback is wanted (it only progresses while connected).
    pub playing: bool,
    /// The playhead has reached the end of the track.
    pub ended: bool,
    pub link: Link,
    /// Volume in percent, once a listener has said what it is.
    pub volume: Option<u8>,
}

pub struct Player {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

struct State {
    pcm: Option<Arc<Vec<u8>>>,
    playing: bool,
    /// Volume in percent, as the listener last reported it.
    volume: Option<u8>,
    /// A volume asked for that the listener has not confirmed yet.
    volume_wanted: Option<u8>,
    /// Counters below are relative to `base` and restart with each epoch.
    base: u64,
    epoch: u32,
    sent: u64,
    consumed: u64,
    played: u64,
    /// Identifies the live connection; 0 when there is none.
    conn: u64,
    link: Link,
    /// Bumped on every change so the sender thread never sleeps through one.
    seq: u64,
    quit: bool,
}

impl State {
    fn total(&self) -> u64 {
        self.pcm.as_ref().map_or(0, |p| (p.len() / FRAME) as u64)
    }

    fn pos(&self) -> u64 {
        (self.base + self.played.min(self.sent)).min(self.total())
    }

    /// Restart the stream from `at`: everything in flight belongs to a dead epoch.
    fn rebase(&mut self, at: u64) {
        self.base = at.min(self.total());
        self.epoch = self.epoch.wrapping_add(1);
        self.sent = 0;
        self.consumed = 0;
        self.played = 0;
    }
}

impl Player {
    /// `targets` are `host:port` addresses to try, in order, until one answers.
    pub fn new(targets: Vec<String>) -> Player {
        let first = targets.first().cloned().unwrap_or_default();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pcm: None,
                playing: false,
                volume: None,
                volume_wanted: None,
                base: 0,
                epoch: 0,
                sent: 0,
                consumed: 0,
                played: 0,
                conn: 0,
                link: Link::Waiting(first),
                seq: 0,
                quit: false,
            }),
            wake: Condvar::new(),
        });
        let worker = shared.clone();
        thread::spawn(move || run(&worker, &targets));
        Player { shared }
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        let mut state = self.shared.lock();
        change(&mut state);
        state.seq += 1;
        self.shared.wake.notify_all();
    }

    /// Start playing a new track from the beginning.
    pub fn load(&self, pcm: Arc<Vec<u8>>) {
        self.update(|s| {
            s.pcm = Some(pcm);
            s.rebase(0);
            s.playing = true;
        });
    }

    pub fn unload(&self) {
        self.update(|s| {
            s.pcm = None;
            s.rebase(0);
        });
    }

    pub fn set_playing(&self, on: bool) {
        self.update(|s| s.playing = on);
    }

    pub fn seek(&self, frame: u64) {
        self.update(|s| s.rebase(frame));
    }

    /// Ask the listener for a volume, in percent.
    pub fn set_volume(&self, percent: u8) {
        self.update(|s| s.volume_wanted = Some(percent.min(100)));
    }

    pub fn status(&self) -> Status {
        let s = self.shared.lock();
        Status {
            pos: s.pos(),
            playing: s.playing,
            ended: s.pcm.is_some() && s.base + s.sent >= s.total() && s.played >= s.sent,
            link: s.link.clone(),
            volume: s.volume_wanted.or(s.volume),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.update(|s| s.quit = true);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleep until something changes, `timeout` passes, or it is time to quit.
    fn wait(&self, seen: u64, timeout: Duration) {
        let guard = self.lock();
        let _ = self
            .wake
            .wait_timeout_while(guard, timeout, |s| s.seq == seen && !s.quit);
    }
}

fn run(shared: &Shared, targets: &[String]) {
    let me = crate::hostname();
    let mut conn = 0u64;
    loop {
        if shared.lock().quit {
            return;
        }
        let (stream, name) = match connect(targets, &me) {
            Ok(found) => found,
            Err(hung_up) => {
                let seen = {
                    let mut s = shared.lock();
                    s.link = match hung_up {
                        Some(addr) => Link::HungUp(addr),
                        None => Link::Waiting(targets.first().cloned().unwrap_or_default()),
                    };
                    s.seq
                };
                shared.wait(seen, Duration::from_secs(1));
                continue;
            }
        };

        conn += 1;
        {
            let mut s = shared.lock();
            let at = s.pos();
            s.rebase(at);
            s.conn = conn;
            s.link = Link::Connected(name);
        }
        if let Ok(reader) = stream.try_clone() {
            thread::scope(|scope| {
                scope.spawn(|| read_reports(shared, reader, conn));
                send(shared, &stream, conn);
                // Also stops the reader.
                let _ = stream.shutdown(Shutdown::Both);
            });
        }

        let mut s = shared.lock();
        // Hold the playhead where it got to until we are connected again.
        let at = s.pos();
        s.rebase(at);
        s.conn = 0;
        s.link = Link::Waiting(targets.first().cloned().unwrap_or_default());
        // Nothing to say about the volume until a listener does.
        s.volume = None;
    }
}

/// The first of `targets` to greet us. Failing that, a target that took the
/// connection and hung up, if any did.
fn connect(targets: &[String], me: &str) -> Result<(TcpStream, String), Option<String>> {
    let mut hung_up = None;
    for target in targets {
        let Ok(addrs) = target.to_socket_addrs() else {
            continue;
        };
        for addr in addrs {
            let Ok(mut stream) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) else {
                continue;
            };
            let hello = (|| {
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(SILENCE_TIMEOUT))?;
                stream.set_write_timeout(Some(SILENCE_TIMEOUT))?;
                proto::write_hello(&mut stream, me)?;
                proto::read_hello(&mut stream)
            })();
            match hello {
                Ok(name) => return Ok((stream, name)),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::InvalidData
                    ) =>
                {
                    hung_up.get_or_insert_with(|| target.clone());
                }
                Err(_) => {}
            }
        }
    }
    Err(hung_up)
}

fn read_reports(shared: &Shared, mut stream: TcpStream, conn: u64) {
    loop {
        let report = Report::read(&mut stream);
        let mut s = shared.lock();
        if s.conn != conn {
            return;
        }
        match report {
            Ok(r) => {
                match s.volume_wanted {
                    // Sent before our change reached the listener.
                    Some(wanted) if r.volume != wanted => {}
                    _ => {
                        s.volume_wanted = None;
                        s.volume = Some(r.volume);
                    }
                }
                if r.epoch != s.epoch {
                    continue; // about audio we have since flushed
                }
                s.consumed = r.consumed;
                s.played = r.played;
            }
            Err(_) => s.conn = 0,
        }
        s.seq += 1;
        shared.wake.notify_all();
        if s.conn != conn {
            return;
        }
    }
}

/// Push state and audio to the listener until the connection is lost.
fn send(shared: &Shared, mut stream: &TcpStream, conn: u64) {
    let mut epoch = None;
    let mut volume_sent = None;
    let mut paused = None;
    let mut last_write = Instant::now();
    let mut out = Vec::new();

    loop {
        out.clear();
        let seen = {
            let mut s = shared.lock();
            if s.quit || s.conn != conn {
                return;
            }
            if epoch != Some(s.epoch) {
                epoch = Some(s.epoch);
                Cmd::Flush(s.epoch).encode(&mut out);
            }
            if let Some(wanted) = s.volume_wanted
                && volume_sent != Some(wanted)
            {
                volume_sent = Some(wanted);
                Cmd::Volume(wanted).encode(&mut out);
            }
            let want_paused = !(s.playing && s.pcm.is_some());
            if paused != Some(want_paused) {
                paused = Some(want_paused);
                Cmd::Pause(want_paused).encode(&mut out);
            }
            // Audio is queued even while paused, so resuming is instant.
            if let Some(pcm) = s.pcm.clone() {
                loop {
                    let at = s.base + s.sent;
                    let left = s.total() - at;
                    let room = WINDOW.saturating_sub(s.sent.saturating_sub(s.consumed));
                    let n = left.min(CHUNK);
                    if n == 0 || n > room {
                        break;
                    }
                    let (from, to) = (at as usize * FRAME, (at + n) as usize * FRAME);
                    proto::encode_audio(&mut out, &pcm[from..to]);
                    s.sent += n;
                }
            }
            s.seq
        };

        if out.is_empty() && last_write.elapsed() > Duration::from_secs(1) {
            Cmd::Ping.encode(&mut out);
        }
        if !out.is_empty() {
            if stream.write_all(&out).is_err() {
                let mut s = shared.lock();
                if s.conn == conn {
                    s.conn = 0;
                }
                return;
            }
            last_write = Instant::now();
        }
        shared.wait(seen, Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listen;

    fn tone(seconds: f32) -> Arc<Vec<u8>> {
        let frames = (seconds * RATE as f32) as usize;
        Arc::new(
            (0..frames * 2)
                .flat_map(|i| ((i % 200) as i16 * 50).to_le_bytes())
                .collect(),
        )
    }

    fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < timeout, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn connected() -> Player {
        connect_to(listen::spawn_local(true).unwrap())
    }

    fn connect_to(addr: std::net::SocketAddr) -> Player {
        let player = Player::new(vec![addr.to_string()]);
        wait_for("connection", Duration::from_secs(5), || {
            matches!(player.status().link, Link::Connected(_))
        });
        player
    }

    // These run against a real clock on machines that may be busy, so they
    // only assert what scheduling delays cannot break: playback never runs
    // ahead of real time, and a paused playhead does not move.

    fn seconds(frames: u64) -> f64 {
        frames as f64 / RATE as f64
    }

    #[test]
    fn plays_through_in_real_time() {
        let player = connected();
        let start = Instant::now();
        player.load(tone(1.0));
        wait_for("playback to start", Duration::from_secs(5), || {
            player.status().pos > 0
        });
        for _ in 0..20 {
            let status = player.status();
            let elapsed = start.elapsed().as_secs_f64();
            assert!(status.playing);
            assert!(
                seconds(status.pos) <= elapsed + 0.05,
                "playhead at {:.3}s after only {elapsed:.3}s",
                seconds(status.pos)
            );
            thread::sleep(Duration::from_millis(20));
        }

        wait_for("the end", Duration::from_secs(10), || player.status().ended);
        let took = start.elapsed().as_secs_f64();
        assert!(took >= 0.95, "a one-second track finished in {took:.2}s");
        assert_eq!(player.status().pos, RATE as u64);
    }

    #[test]
    fn pause_holds_the_playhead_and_seek_moves_it() {
        let player = connected();
        player.load(tone(10.0));
        wait_for("playback to start", Duration::from_secs(5), || {
            player.status().pos > 0
        });
        player.set_playing(false);
        // The pause reaches the listener a moment later; wait for it to bite.
        let mut held = player.status().pos;
        wait_for("the pause to take effect", Duration::from_secs(5), || {
            thread::sleep(Duration::from_millis(150));
            let now = player.status().pos;
            let still = now == held;
            held = now;
            still
        });
        thread::sleep(Duration::from_millis(300));
        assert_eq!(player.status().pos, held, "playhead moved while paused");

        // Seeking while paused lands exactly and stays there.
        let five = 5 * RATE as u64;
        player.seek(five);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(player.status().pos, five);

        let resumed = Instant::now();
        player.set_playing(true);
        wait_for("playback to resume", Duration::from_secs(5), || {
            player.status().pos > five
        });
        let pos = player.status().pos;
        let elapsed = resumed.elapsed().as_secs_f64();
        assert!(
            seconds(pos - five) <= elapsed + 0.05,
            "moved {:.3}s in {elapsed:.3}s",
            seconds(pos - five)
        );

        // Seeking past the end clamps to it.
        player.seek(u64::MAX);
        let s = player.status();
        assert!(s.ended);
        assert_eq!(s.pos, 10 * RATE as u64);
    }

    #[test]
    fn waits_for_a_listener_and_survives_losing_it() {
        // Nothing is listening here yet.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let player = Player::new(vec![format!("127.0.0.1:{port}")]);
        player.load(tone(1.0));
        thread::sleep(Duration::from_millis(300));
        let s = player.status();
        assert_eq!(s.link, Link::Waiting(format!("127.0.0.1:{port}")));
        assert_eq!(s.pos, 0, "nothing plays without a listener");
        assert!(!s.ended);

        // A listener that greets us and then vanishes.
        let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (mut peer, _) = l.accept().unwrap();
        proto::read_hello(&mut peer).unwrap();
        proto::write_hello(&mut peer, "flaky").unwrap();
        wait_for("connection", Duration::from_secs(5), || {
            player.status().link == Link::Connected("flaky".into())
        });
        drop(peer);
        drop(l);
        wait_for("disconnect", Duration::from_secs(5), || {
            matches!(player.status().link, Link::Waiting(_))
        });
    }

    #[test]
    fn says_when_the_listener_hangs_up_without_a_word() {
        // What an older listener does with a hello it does not understand.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for peer in l.incoming() {
                drop(peer);
            }
        });
        let player = Player::new(vec![addr.clone()]);
        wait_for("the hang-up", Duration::from_secs(5), || {
            player.status().link == Link::HungUp(addr.clone())
        });
    }

    #[test]
    fn volume_is_the_listeners_and_outlives_the_player() {
        let dir = std::env::temp_dir().join(format!("sshound-player-{}", std::process::id()));
        let file = dir.join("volume");
        let addr = listen::spawn_null_saving(file.clone()).unwrap();

        let first = connect_to(addr);
        wait_for("the listener's volume", Duration::from_secs(5), || {
            first.status().volume == Some(100)
        });
        first.set_volume(40);
        assert_eq!(first.status().volume, Some(40), "shown straight away");
        wait_for("the listener to save it", Duration::from_secs(5), || {
            std::fs::read_to_string(&file).is_ok_and(|text| text.trim() == "40")
        });
        thread::sleep(Duration::from_millis(600));
        assert_eq!(first.status().volume, Some(40), "confirmed, not undone");
        drop(first);

        // The next player hears it at the level the last one left.
        let second = connect_to(addr);
        wait_for("the saved volume", Duration::from_secs(5), || {
            second.status().volume == Some(40)
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
