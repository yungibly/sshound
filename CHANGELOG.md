# Changelog

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
