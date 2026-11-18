# sshound

Play audio files from a remote shell and hear them on your own machine.

You run `sshound some-file.flac` in an ssh session. The remote machine draws
the waveform and controls in your terminal and streams the audio straight back
to the machine you are sitting at, which plays it.

It is one binary with two roles:

| Where | Command | What it does |
|---|---|---|
| Remote machine | `sshound FILE...` | Decodes, draws the player, sends audio |
| Your machine | `sshound listen` | Receives audio and plays it |

## Setup

Build it on both machines (`cargo install --path .` puts it in `~/.cargo/bin`).

- **Remote machine:** needs `ffmpeg` on the `PATH` for decoding.
- **Your machine (macOS):** needs nothing else; it plays through the default
  output device. On Linux it plays through `pw-cat` or `aplay`.

On your machine, leave this running:

```sh
sshound listen
```

Then ssh to the remote machine over Tailscale and play something:

```sh
sshound kick.wav
sshound album/*        # several files; things that are not audio are skipped
```

## Controls

| Key | Action |
|---|---|
| `space` | Play / pause |
| `←` `→` | Seek (5 s, or a tenth of the file for short sounds) |
| `0` | Back to the start |
| `↑` `↓` | Volume |
| `n` `p` | Next / previous file |
| `q` | Quit |

Clicking or dragging on the waveform seeks; the buttons are clickable too.

## How the two ends find each other

The player connects to port 7746 on the address your ssh session came from
(`$SSH_CONNECTION`), so over Tailscale there is nothing to configure. The
listener only accepts connections on its Tailscale address and on localhost.

If that does not fit your setup:

- `sshound --to HOST[:PORT] FILE` (or `$SSHOUND_TO`) names the listener explicitly.
- `sshound listen --bind ADDR` listens somewhere else, e.g. `--bind 0.0.0.0`
  if you ssh over the LAN rather than Tailscale.
- Without Tailscale, forward the port instead: `ssh -R 7746:localhost:7746 host`.
  The player falls back to localhost when the direct address does not answer.

Run outside an ssh session, `sshound FILE` just plays on the machine it is on.

If the player says it is waiting for `sshound listen`, the listener is not
running or not reachable at the address shown. On macOS, the firewall may ask
whether to allow incoming connections the first time; say yes.

## Good to know

- Audio travels as uncompressed 48 kHz stereo, about 1.5 Mbit/s. That is fine on
  a direct Tailscale connection and may stutter over a slow relayed one.
- The whole file is decoded into memory before it plays: roughly 11 MB and a
  tenth of a second per minute of audio.
- There is no authentication beyond the network: anyone who can reach the
  listener's port (by default, devices on your tailnet) can play sound through it.
