mod decode;
mod listen;
mod output;
mod player;
mod proto;
mod source;
mod ui;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

const HELP: &str = "\
sshound: play audio files from a remote shell and hear them on your own machine

  sshound FILE...       open the player         (on the remote machine)
  sshound listen        receive and play audio  (on the machine with the speakers)

Player options:
  --to HOST[:PORT]      where the listener is. Default: the machine you ssh'd in
                        from, then localhost. Also read from $SSHOUND_TO.

Listener options:
  --port N              port to listen on (default 7746)
  --bind ADDR           address to listen on; repeatable
                        (default: this machine's Tailscale address, and localhost)
  --null                discard the audio instead of playing it

Keys:
  space  play/pause     ← →  seek          0  back to start
  ↑ ↓    volume         n p  next/previous file          q  quit
  Click or drag on the waveform to seek.
";

fn main() {
    if let Err(e) = real_main() {
        eprintln!("sshound: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        print!("{HELP}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("sshound {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args[0] == "listen" {
        args.remove(0);
        listen(args)
    } else {
        play(args)
    }
}

/// Named options as (name, value) pairs, and everything else.
type Parsed = (Vec<(String, String)>, Vec<OsString>);

/// Split `--name=value` / `--name value` style arguments from positional ones.
fn parse(args: Vec<OsString>, flags: &[&str], options: &[&str]) -> Result<Parsed> {
    let mut named = Vec::new();
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        if text == "--" {
            positional.extend(args.by_ref());
        } else if text.starts_with("--") {
            let (name, inline) = match text.split_once('=') {
                Some((name, value)) => (name.to_string(), Some(value.to_string())),
                None => (text, None),
            };
            if flags.contains(&name.as_str()) && inline.is_none() {
                named.push((name, String::new()));
            } else if options.contains(&name.as_str()) {
                let value = match inline.or_else(|| args.next().map(|v| v.to_string_lossy().into_owned())) {
                    Some(value) => value,
                    None => bail!("{name} needs a value"),
                };
                named.push((name, value));
            } else {
                bail!("unknown option {name} (see --help)");
            }
        } else {
            positional.push(arg);
        }
    }
    Ok((named, positional))
}

fn listen(args: Vec<OsString>) -> Result<()> {
    let (named, positional) = parse(args, &["--null"], &["--port", "--bind"])?;
    if let Some(extra) = positional.first() {
        bail!("unexpected argument {extra:?} (see --help)");
    }
    let mut opts = listen::Opts { port: proto::PORT, bind: Vec::new(), null: false };
    for (name, value) in named {
        match name.as_str() {
            "--null" => opts.null = true,
            "--port" => opts.port = value.parse().with_context(|| format!("bad port {value:?}"))?,
            _ => opts.bind.push(value.parse().with_context(|| format!("bad address {value:?}"))?),
        }
    }
    listen::run(opts)
}

fn play(args: Vec<OsString>) -> Result<()> {
    let (named, positional) = parse(args, &[], &["--to"])?;
    let to = named.into_iter().map(|(_, value)| value).next_back();

    let mut files = Vec::new();
    for arg in positional {
        let path = PathBuf::from(arg);
        match path.metadata() {
            Ok(meta) if meta.is_dir() => {} // so that `sshound *` works
            Ok(_) => files.push(path),
            Err(e) => bail!("{}: {e}", path.display()),
        }
    }
    if files.is_empty() {
        bail!("no files to play (see --help)");
    }
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("the player needs a terminal");
    }

    let targets = match to.or_else(|| std::env::var("SSHOUND_TO").ok().filter(|s| !s.is_empty())) {
        Some(to) => vec![with_port(&to)],
        None => match ssh_client() {
            // The listener binds localhost too, so an `ssh -R` tunnel works where
            // the client is not directly reachable.
            Some(ip) => vec![
                SocketAddr::new(ip, proto::PORT).to_string(),
                format!("127.0.0.1:{}", proto::PORT),
            ],
            // Not in an ssh session: just play here.
            None => {
                let addr = listen::spawn_local(false).context(
                    "not in an ssh session and cannot play audio on this machine; \
                     use --to HOST to send it to a machine running `sshound listen`",
                )?;
                vec![addr.to_string()]
            }
        },
    };

    ui::run(files, player::Player::new(targets))
}

/// The address of the machine at the other end of this ssh session.
fn ssh_client() -> Option<IpAddr> {
    let conn = std::env::var("SSH_CONNECTION").or_else(|_| std::env::var("SSH_CLIENT")).ok()?;
    let ip = conn.split_whitespace().next()?;
    // Link-local IPv6 addresses carry a %scope that IpAddr cannot parse.
    ip.split('%').next()?.parse().ok()
}

fn with_port(to: &str) -> String {
    if let Ok(ip) = to.parse::<IpAddr>() {
        SocketAddr::new(ip, proto::PORT).to_string()
    } else if to.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok()) {
        to.to_string()
    } else {
        format!("{to}:{}", proto::PORT)
    }
}

/// This machine's short hostname, for showing at the other end.
pub fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|name| name.trim().split('.').next().map(str::to_string))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_addresses_get_the_default_port() {
        assert_eq!(with_port("100.64.0.7"), "100.64.0.7:7746");
        assert_eq!(with_port("fd7a:115c:a1e0::1"), "[fd7a:115c:a1e0::1]:7746");
        assert_eq!(with_port("studio-mac"), "studio-mac:7746");
        assert_eq!(with_port("studio-mac:9000"), "studio-mac:9000");
        assert_eq!(with_port("[::1]:9000"), "[::1]:9000");
    }

    #[test]
    fn arguments_split_into_options_and_files() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let (named, files) = parse(args(&["a.wav", "--to", "mac", "b.wav"]), &[], &["--to"]).unwrap();
        assert_eq!(named, [("--to".to_string(), "mac".to_string())]);
        assert_eq!(files, args(&["a.wav", "b.wav"]));

        let (named, files) = parse(args(&["--to=mac:1", "--", "--odd.wav"]), &[], &["--to"]).unwrap();
        assert_eq!(named, [("--to".to_string(), "mac:1".to_string())]);
        assert_eq!(files, args(&["--odd.wav"]));

        assert!(parse(args(&["--to"]), &[], &["--to"]).is_err());
        assert!(parse(args(&["--nope"]), &[], &["--to"]).is_err());
    }
}
