//! Wire protocol between the player (on the remote machine) and the listener
//! (on the machine with the speakers).
//!
//! The player connects, both sides exchange a hello, then the player sends
//! framed commands while the listener streams back fixed-size position reports.
//! Audio is always 48 kHz stereo signed 16-bit little-endian.

use std::io::{self, Read, Write};

pub const PORT: u16 = 7746;
pub const RATE: u32 = 48_000;
/// Bytes per audio frame (two s16 samples).
pub const FRAME: usize = 4;

const MAGIC: &[u8; 8] = b"sshound1";
const MAX_PAYLOAD: usize = 1 << 20;

const T_AUDIO: u8 = 1;
const T_FLUSH: u8 = 2;
const T_PAUSE: u8 = 3;
const T_VOLUME: u8 = 4;
const T_PING: u8 = 5;

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub fn write_hello(w: &mut impl Write, name: &str) -> io::Result<()> {
    let name = &name.as_bytes()[..name.len().min(255)];
    let mut buf = Vec::with_capacity(MAGIC.len() + 1 + name.len());
    buf.extend_from_slice(MAGIC);
    buf.push(name.len() as u8);
    buf.extend_from_slice(name);
    w.write_all(&buf)
}

pub fn read_hello(r: &mut impl Read) -> io::Result<String> {
    let mut head = [0u8; 9];
    r.read_exact(&mut head)?;
    if &head[..8] != MAGIC {
        return Err(bad("not an sshound peer"));
    }
    let mut name = vec![0u8; head[8] as usize];
    r.read_exact(&mut name)?;
    // The name is shown on the other side's terminal: keep it short and printable.
    Ok(String::from_utf8_lossy(&name).chars().filter(|c| !c.is_control()).take(40).collect())
}

/// Player → listener.
#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// PCM to append to the listener's queue.
    Audio(Vec<u8>),
    /// Drop everything queued and start a new epoch (sent on seek).
    Flush(u32),
    Pause(bool),
    /// Linear gain, 0..=1.
    Volume(f32),
    Ping,
}

fn header(out: &mut Vec<u8>, tag: u8, len: usize) {
    out.push(tag);
    out.extend_from_slice(&(len as u32).to_le_bytes());
}

/// Append an audio command without allocating an intermediate `Cmd`.
pub fn encode_audio(out: &mut Vec<u8>, pcm: &[u8]) {
    header(out, T_AUDIO, pcm.len());
    out.extend_from_slice(pcm);
}

impl Cmd {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Cmd::Audio(pcm) => encode_audio(out, pcm),
            Cmd::Flush(epoch) => {
                header(out, T_FLUSH, 4);
                out.extend_from_slice(&epoch.to_le_bytes());
            }
            Cmd::Pause(on) => {
                header(out, T_PAUSE, 1);
                out.push(*on as u8);
            }
            Cmd::Volume(v) => {
                header(out, T_VOLUME, 4);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Cmd::Ping => header(out, T_PING, 0),
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<Cmd> {
        let mut head = [0u8; 5];
        r.read_exact(&mut head)?;
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(bad("oversized message"));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body)?;
        match (head[0], len) {
            (T_AUDIO, n) if n % FRAME == 0 => Ok(Cmd::Audio(body)),
            (T_FLUSH, 4) => Ok(Cmd::Flush(u32::from_le_bytes([
                body[0], body[1], body[2], body[3],
            ]))),
            (T_PAUSE, 1) => Ok(Cmd::Pause(body[0] != 0)),
            (T_VOLUME, 4) => Ok(Cmd::Volume(f32::from_le_bytes([
                body[0], body[1], body[2], body[3],
            ]))),
            (T_PING, 0) => Ok(Cmd::Ping),
            _ => Err(bad("malformed message")),
        }
    }
}

/// Listener → player: where playback actually is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Epoch these counters belong to (they restart at zero on every flush).
    pub epoch: u32,
    /// Frames taken off the queue so far. Drives flow control.
    pub consumed: u64,
    /// Frames that have reached the speakers. Drives the playhead.
    pub played: u64,
}

impl Report {
    pub const SIZE: usize = 20;

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[..4].copy_from_slice(&self.epoch.to_le_bytes());
        b[4..12].copy_from_slice(&self.consumed.to_le_bytes());
        b[12..].copy_from_slice(&self.played.to_le_bytes());
        b
    }

    pub fn read(r: &mut impl Read) -> io::Result<Report> {
        let mut b = [0u8; Self::SIZE];
        r.read_exact(&mut b)?;
        Ok(Report {
            epoch: u32::from_le_bytes(b[..4].try_into().unwrap()),
            consumed: u64::from_le_bytes(b[4..12].try_into().unwrap()),
            played: u64::from_le_bytes(b[12..].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip() {
        let cmds = [
            Cmd::Audio(vec![1, 2, 3, 4, 5, 6, 7, 8]),
            Cmd::Flush(7),
            Cmd::Pause(true),
            Cmd::Volume(0.25),
            Cmd::Ping,
        ];
        let mut buf = Vec::new();
        for c in &cmds {
            c.encode(&mut buf);
        }
        let mut r = &buf[..];
        for c in &cmds {
            assert_eq!(&Cmd::read(&mut r).unwrap(), c);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn rejects_garbage() {
        // Audio that is not a whole number of frames.
        let mut buf = Vec::new();
        encode_audio(&mut buf, &[0, 0, 0]);
        assert!(Cmd::read(&mut &buf[..]).is_err());
        // Unknown tag.
        assert!(Cmd::read(&mut &[99u8, 0, 0, 0, 0][..]).is_err());
        // A length that would make us allocate gigabytes.
        assert!(Cmd::read(&mut &[T_AUDIO, 0xff, 0xff, 0xff, 0x7f][..]).is_err());
        // Wrong greeting.
        assert!(read_hello(&mut &b"GET / HTTP/1.1\r\n"[..]).is_err());
    }

    #[test]
    fn hello_and_report_round_trip() {
        let mut buf = Vec::new();
        write_hello(&mut buf, "workbox").unwrap();
        assert_eq!(read_hello(&mut &buf[..]).unwrap(), "workbox");

        buf.clear();
        write_hello(&mut buf, "evil\x1b[2J\nname").unwrap();
        assert_eq!(read_hello(&mut &buf[..]).unwrap(), "evil[2Jname");

        let r = Report { epoch: 3, consumed: 1 << 40, played: 12345 };
        assert_eq!(Report::read(&mut &r.to_bytes()[..]).unwrap(), r);
    }
}
