#!/usr/bin/env python3
"""Build and smoke-test a native release archive. All artifacts stay under target/."""
import argparse
import hashlib
from pathlib import Path
import shutil
import socket
import subprocess
import tarfile
import tempfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def smoke_test(binary, version):
    """Check the extracted binary: its version, and that a listener greets a player."""
    actual = subprocess.check_output([str(binary), "--version"])
    assert actual == f"sshound {version}\n".encode(), actual
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    # --null discards the audio, so this works on machines with no sound device.
    listener = subprocess.Popen(
        [str(binary), "listen", "--null", "--bind", "127.0.0.1", "--port", str(port)],
        stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while True:
            try:
                peer = socket.create_connection(("127.0.0.1", port), timeout=5)
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)
        with peer:
            peer.sendall(b"sshound1\x07package")
            reply = peer.recv(8, socket.MSG_WAITALL)
            assert reply == b"sshound1", reply
    finally:
        listener.terminate()
        listener.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Rust target; its binary must run on this machine")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    rust = subprocess.check_output(["rustc", "-vV"], text=True, cwd=ROOT)
    host = next(line.removeprefix("host: ") for line in rust.splitlines() if line.startswith("host: "))
    target = args.target or host
    subprocess.run(["cargo", "build", "--release", "--locked", "--target", target], cwd=ROOT, check=True)
    binary = ROOT / "target" / target / "release/sshound"
    output = ROOT / "target/packages"
    output.mkdir(parents=True, exist_ok=True)
    name = f"sshound-{version}-{target}"
    with tempfile.TemporaryDirectory(dir=output, prefix="stage-") as temp:
        stage = Path(temp) / name
        (stage / "bin").mkdir(parents=True)
        shutil.copy2(binary, stage / "bin/sshound")
        shutil.copy2(ROOT / "README.md", stage / "README.md")
        (stage / "BUILD.txt").write_text(f"sshound {version}\npackage target: {target}\n{rust}")
        archive = output / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(stage, arcname=name)
        # Exercise the artifact itself, after extraction to a different prefix.
        unpacked = Path(temp) / "unpacked"
        with tarfile.open(archive) as bundle:
            bundle.extractall(unpacked, filter="data")
        smoke_test(unpacked / name / "bin/sshound", version)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest}  {archive.name}\n")
    print(f"{archive}\nSHA-256 {digest}\nExtracted binary: version and listener greeting checks passed.")


if __name__ == "__main__":
    main()
