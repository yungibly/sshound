#!/usr/bin/env python3
"""Capture sshound's interface in a headless terminal, to look at.

Runs the player under the pinned tui-test binary against a listener that
discards the audio, puts it through its states, and writes a PNG and the plain
text of each one. Needs ffmpeg, for the test sounds.

    cargo build
    python3 tools/tui-test/shots.py target/shots
"""
import argparse
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]

# A 42-second "song" with a quiet intro, two loud sections and a break; a short hit.
SONG = ("aevalsrc='(0.12+0.55*between(t,8,20)+0.8*between(t,24,36)+0.3*between(t,36,40))"
        "*(0.55+0.45*sin(2*PI*0.37*t))*sin(2*PI*110*t)*exp(-4*mod(t,0.5))':s=44100:d=42")
HIT = "aevalsrc='0.9*sin(2*PI*180*t*exp(-3*t))*exp(-6*t)':s=44100:d=0.8"


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class Terminal:
    def __init__(self, tool, binary, output, home):
        self.tool, self.binary, self.output = tool, binary, output
        # Variables describing the outer terminal or session would not describe this one.
        dropped = ("NO_COLOR", "TMUX", "SSH_CONNECTION", "SSH_CLIENT", "SSHOUND_TO")
        self.env = {k: v for k, v in os.environ.items() if k not in dropped}
        self.env.update(TUI_TEST_HOME=str(home), TUI_TEST_SESSION="sshound")

    def tui(self, *args):
        result = subprocess.run([self.tool, *map(str, args)], env=self.env,
                                capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise SystemExit(f"tui-test {' '.join(map(str, args))}:\n{result.stdout}{result.stderr}")
        return result.stdout

    def play(self, cols, rows, listener, *files):
        self.tui("run", "--restart", "--cols", cols, "--rows", rows,
                 "--env", f"SSHOUND_TO={listener}", "--env", "TERM=xterm-256color",
                 self.binary, *files)

    def shot(self, name, *expected, settle=True):
        for text in expected:
            self.tui("expect", "text", text)
        if settle:
            self.tui("wait", "idle", "--timeout", 3000)
        self.tui("screenshot", "-o", self.output / f"{name}.png")
        (self.output / f"{name}.txt").write_text(self.tui("text"))
        print(f"{name}.png")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("output", type=Path, help="directory for the captures")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/sshound")
    parser.add_argument("--tool", type=Path, help="tui-test binary (default: fetch the pinned one)")
    args = parser.parse_args()
    tool = args.tool or Path(subprocess.check_output(
        ["sh", str(ROOT / "tools/tui-test/bootstrap.sh")], text=True, stderr=subprocess.DEVNULL).strip())
    binary = args.binary.resolve()
    args.output.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="sshound-shots-") as scratch:
        scratch = Path(scratch)
        song, hit, notes = scratch / "song.flac", scratch / "hit.wav", scratch / "notes.txt"
        for source, path in ((SONG, song), (HIT, hit)):
            subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", source, "-ac", "2", str(path)], check=True)
        notes.write_text("not audio\n")

        port = free_port()
        listener = subprocess.Popen([binary, "listen", "--null", "--bind", "127.0.0.1", "--port", str(port)],
                                    stderr=subprocess.DEVNULL)
        term = Terminal(tool, binary, args.output.resolve(), scratch / "tui-test")
        try:
            here, nowhere = f"127.0.0.1:{port}", f"127.0.0.1:{free_port()}"

            term.play(100, 26, here, song)
            term.tui("expect", "text", "/ 0:42")
            time.sleep(1.2)
            term.shot("playing", "▌▌")

            # Paused, a click on the waveform lands on an exact position.
            term.tui("key", "press", "Space")
            term.tui("mouse", "click", 40, 12)
            term.shot("paused", "0:16 / 0:42", "▶")

            # The volume bar shows for a moment after a change.
            term.tui("key", "press", "Down", "Down", "Down", "Down")
            term.shot("volume", "━", settle=False)

            for cols, rows in ((60, 12), (44, 9), (30, 4)):
                term.tui("resize", cols, rows)
                term.shot(f"size-{cols}x{rows}", "song.flac")

            term.play(100, 26, here, song, notes, hit)
            term.tui("key", "press", "Space")
            term.shot("list", "1/3")

            term.play(100, 26, nowhere, song)
            term.shot("waiting", "/ 0:42", f"waiting for `sshound listen` on {nowhere}")

            term.play(100, 26, here, notes)
            term.shot("failed", "Invalid data")

            # Through this machine's own speakers (the listener is ignored).
            term.play(100, 26, here, "--here", song)
            term.shot("local", "▌▌", "playing on this machine")
        finally:
            subprocess.run([tool, "close", "--all"], env=term.env, capture_output=True)
            subprocess.run([tool, "daemon", "stop", "--all"], env=term.env, capture_output=True)
            listener.terminate()
            listener.wait()


if __name__ == "__main__":
    main()
