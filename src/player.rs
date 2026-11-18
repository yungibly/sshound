//! The playback engine on the remote side: holds the decoded track, keeps a
//! connection to the listener, and feeds it audio a little ahead of the playhead.
//!
//! The listener's audio device is the clock. It reports how much it has taken
//! and how much has been heard; we top its queue up to `WINDOW` and show the
//! heard position. Pause, volume and seek are commands, so they act at once
//! however much audio is queued.

use std::io::Write;
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
    volume: f32,
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
                volume: 1.0,
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

    /// Linear gain, 0..=1.
    pub fn set_volume(&self, volume: f32) {
        self.update(|s| s.volume = volume.clamp(0.0, 1.0));
    }

    pub fn status(&self) -> Status {
        let s = self.shared.lock();
        Status {
            pos: s.pos(),
            playing: s.playing,
            ended: s.pcm.is_some() && s.base + s.sent >= s.total() && s.played >= s.sent,
            link: s.link.clone(),
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
        let _ = self.wake.wait_timeout_while(guard, timeout, |s| s.seq == seen && !s.quit);
    }
}

fn run(shared: &Shared, targets: &[String]) {
    let me = crate::hostname();
    let mut conn = 0u64;
    loop {
        if shared.lock().quit {
            return;
        }
        let Some((stream, name)) = connect(targets, &me) else {
            let seen = shared.lock().seq;
            shared.wait(seen, Duration::from_secs(1));
            continue;
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
    }
}

fn connect(targets: &[String], me: &str) -> Option<(TcpStream, String)> {
    for target in targets {
        let Ok(addrs) = target.to_socket_addrs() else { continue };
        for addr in addrs {
            let Ok(mut stream) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) else { continue };
            let hello = (|| {
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(SILENCE_TIMEOUT))?;
                stream.set_write_timeout(Some(SILENCE_TIMEOUT))?;
                proto::write_hello(&mut stream, me)?;
                proto::read_hello(&mut stream)
            })();
            if let Ok(name) = hello {
                return Some((stream, name));
            }
        }
    }
    None
}

fn read_reports(shared: &Shared, mut stream: TcpStream, conn: u64) {
    loop {
        let report = Report::read(&mut stream);
        let mut s = shared.lock();
        if s.conn != conn {
            return;
        }
        match report {
            Ok(r) if r.epoch == s.epoch => {
                s.consumed = r.consumed;
                s.played = r.played;
            }
            Ok(_) => continue, // about audio we have since flushed
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
    let mut volume = None;
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
            if volume != Some(s.volume) {
                volume = Some(s.volume);
                Cmd::Volume(s.volume).encode(&mut out);
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
        Arc::new((0..frames * 2).flat_map(|i| ((i % 200) as i16 * 50).to_le_bytes()).collect())
    }

    fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < timeout, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn connected() -> Player {
        let addr = listen::spawn_local(true).unwrap();
        let player = Player::new(vec![addr.to_string()]);
        wait_for("connection", Duration::from_secs(5), || {
            matches!(player.status().link, Link::Connected(_))
        });
        player
    }

    #[test]
    fn plays_through_in_real_time() {
        let player = connected();
        let start = Instant::now();
        player.load(tone(1.0));
        thread::sleep(Duration::from_millis(500));
        let mid = player.status();
        assert!(mid.playing && !mid.ended);
        let secs = mid.pos as f64 / RATE as f64;
        assert!((0.3..0.6).contains(&secs), "half a second in, playhead at {secs:.2}s");

        wait_for("the end", Duration::from_secs(5), || player.status().ended);
        let took = start.elapsed().as_secs_f64();
        assert!((0.9..1.4).contains(&took), "a one-second track took {took:.2}s");
        assert_eq!(player.status().pos, RATE as u64);
    }

    #[test]
    fn pause_holds_the_playhead_and_seek_moves_it() {
        let player = connected();
        player.load(tone(10.0));
        thread::sleep(Duration::from_millis(300));
        player.set_playing(false);
        thread::sleep(Duration::from_millis(200));
        let held = player.status().pos;
        assert!(held > 0);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(player.status().pos, held, "playhead moved while paused");

        // Seeking while paused lands exactly and stays there.
        player.seek(5 * RATE as u64);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(player.status().pos, 5 * RATE as u64);

        player.set_playing(true);
        thread::sleep(Duration::from_millis(400));
        let secs = player.status().pos as f64 / RATE as f64;
        assert!((5.2..5.5).contains(&secs), "resumed from 5s, now at {secs:.2}s");

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
}
