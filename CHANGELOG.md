# Changelog

## v0.2.0

- Without an ssh session to go by, the player looks for `sshound listen` on the
  Tailscale devices this machine is talking to before it falls back to playing
  where it is. Terminals that start shells on the remote machine without going
  through ssh now send the sound to you, as an ssh session does.
- When the sound plays on the machine the player runs on, the player says so
  next to the file name.
- `--here` plays on this machine without looking for a listener.
- The volume is always on screen, and the machine with the speakers remembers
  it: playing there from anywhere, or on that machine itself, starts at the
  level you last left it.
- The listener no longer logs connections that close without a word.
- The protocol has changed, so update both ends together. A player that meets
  an older listener says the listener hung up instead of waiting in silence.

## v0.1.0

First release.

- `sshound FILE...` plays audio files from a remote shell: a bar waveform and
  transport controls in the terminal, sound on the machine you ssh'd in from.
- `sshound listen` receives and plays the audio, through the default output on
  macOS and through `pw-cat` or `aplay` on Linux.
- The player finds the listener from the ssh session, over Tailscale or an
  `ssh -R` tunnel, and reconnects by itself if the listener goes away.
- Keyboard and mouse control: play/pause, seek, volume, next/previous file,
  click or drag on the waveform.
