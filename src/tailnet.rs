//! What Tailscale knows: this machine's addresses, and which devices it is
//! talking to, for finding a listener when there is no ssh session to go by.

use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Where the Tailscale CLI may be: on the `PATH`, or inside the macOS app.
const CLIS: &[&str] = &[
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// This machine's Tailscale addresses, found by asking the OS which local
/// address it would use to reach Tailscale's own service address. No packets
/// are sent.
pub fn addrs() -> Vec<IpAddr> {
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

/// A listener on one of the Tailscale devices this machine has been talking
/// to in the last few minutes. The machine you are sitting at is one of them
/// whenever your terminal session runs over Tailscale, even when the session
/// does not come through ssh (and so leaves no `$SSH_CONNECTION`).
pub fn find_listener(port: u16) -> Option<SocketAddr> {
    let peers: Vec<SocketAddr> = active_peers()
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    first_answer(&peers, PROBE_TIMEOUT)
}

/// Addresses of the peers `tailscale status --active` lists.
fn active_peers() -> Vec<IpAddr> {
    CLIS.iter()
        .find_map(|cli| run(cli, &["status", "--active", "--self=false"]))
        .map_or_else(Vec::new, |out| parse_status(&out))
}

/// Each line of `tailscale status` starts with the device's address; other
/// lines (health warnings, blank ones) do not.
fn parse_status(text: &str) -> Vec<IpAddr> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

/// The standard output of a command that succeeds within `STATUS_TIMEOUT`.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let (tx, rx) = mpsc::channel();
    // A wedged tailscaled must not hold the player up: give up on the command
    // and leave this thread to collect it whenever it finishes.
    thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    let out = rx.recv_timeout(STATUS_TIMEOUT).ok()?.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The first of `addrs` to accept a TCP connection within `timeout`. They are
/// all tried at once; the connections are closed straight away.
fn first_answer(addrs: &[SocketAddr], timeout: Duration) -> Option<SocketAddr> {
    let (tx, rx) = mpsc::channel();
    for &addr in addrs {
        let tx = tx.clone();
        thread::spawn(move || {
            let answered = TcpStream::connect_timeout(&addr, timeout).is_ok();
            let _ = tx.send(answered.then_some(addr));
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    // Ends early once every attempt has failed.
    while let Ok(answer) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if answer.is_some() {
            return answer;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn reads_peer_addresses_from_status() {
        let status = "\
100.64.0.7     studio-mac   someone@  macOS  active; direct 192.0.2.10:41641, tx 4018 rx 1337
100.64.0.9     phone        someone@  iOS    active; relay \"nyc\", tx 12 rx 40

# Health check:
#     - Some peers are advertising routes but --accept-routes is false
";
        assert_eq!(
            parse_status(status),
            [
                "100.64.0.7".parse::<IpAddr>().unwrap(),
                "100.64.0.9".parse().unwrap()
            ]
        );
        assert!(parse_status("Tailscale is stopped.\n").is_empty());
    }

    #[test]
    fn finds_whichever_address_answers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let timeout = Duration::from_secs(5);
        assert_eq!(first_answer(&[closed, open], timeout), Some(open));

        // Gives up as soon as everything has refused, without waiting out the timeout.
        let start = Instant::now();
        assert_eq!(first_answer(&[closed], timeout), None);
        assert_eq!(first_answer(&[], timeout), None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
