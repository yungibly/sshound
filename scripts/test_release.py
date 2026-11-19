"""Tests for release validation and formula generation."""
import hashlib
import re
from pathlib import Path
import tempfile
import unittest

import release


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        self.changelog = self.root / "CHANGELOG.md"
        self.changelog.write_text("# Changelog\n\n## v1.2.3\n\nSecond.\n\n## v1.2.2\n\nFirst.\n")

    def packages(self, version="1.2.3"):
        packages = self.root / "packages"
        packages.mkdir()
        for targets in release.TARGETS.values():
            for target in targets.values():
                archive = packages / f"sshound-{version}-{target}.tar.gz"
                archive.write_bytes(target.encode())
                digest = hashlib.sha256(target.encode()).hexdigest()
                (packages / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n")
        return packages

    def test_tag_must_be_a_plain_version_matching_cargo(self):
        release.validate_tag("v1.2.3", "1.2.3")
        for tag in ("1.2.3", "v1.2", "v1.2.3-rc1", "v01.2.3", "v1.2.4"):
            with self.assertRaises(ValueError, msg=tag):
                release.validate_tag(tag, "1.2.3")

    def test_notes_are_the_section_for_that_version(self):
        self.assertEqual(release.release_notes("1.2.3", self.changelog), "Second.")
        self.assertEqual(release.release_notes("1.2.2", self.changelog), "First.")
        with self.assertRaises(ValueError):
            release.release_notes("9.9.9", self.changelog)

    def test_formula_points_at_every_archive_with_its_checksum(self):
        packages, output = self.packages(), self.root / "metadata"
        release.prepare("v1.2.3", "1.2.3", "someone/sshound", packages, output, self.changelog)
        formula = (output / "sshound.rb").read_text()
        self.assertIn("class Sshound < Formula", formula)
        self.assertIn('version "1.2.3"', formula)
        self.assertEqual(formula.count("on_arm do"), 2)
        self.assertEqual(formula.count("on_intel do"), 2)
        for targets in release.TARGETS.values():
            for target in targets.values():
                name = f"sshound-1.2.3-{target}.tar.gz"
                digest = hashlib.sha256(target.encode()).hexdigest()
                self.assertIn(f'url "https://github.com/someone/sshound/releases/download/v1.2.3/{name}"', formula)
                self.assertIn(f'sha256 "{digest}"', formula)
                self.assertIn(f"{digest}  {name}", (output / "SHA256SUMS").read_text())
        # Ruby escapes and interpolation must reach the formula untouched.
        self.assertIn(r'assert_equal "sshound #{version}\n"', formula)
        self.assertIn(r'socket.write "sshound2\x04brew"', formula)
        self.assertTrue(formula.endswith("end\n"))
        self.assertIn("Second.", (output / "notes.md").read_text())

    def test_smoke_tests_speak_the_current_protocol(self):
        # The formula test and the packaging check greet a real listener; an
        # out-of-date greeting would fail the release after it is published.
        root = Path(__file__).resolve().parents[1]
        proto = (root / "src/proto.rs").read_text()
        magic = re.search(r'const MAGIC: &\[u8; 8\] = b"(sshound\d)";', proto).group(1)
        for script in ("release.py", "package.py"):
            text = (root / "scripts" / script).read_text()
            self.assertEqual(set(re.findall(r"sshound\d", text)), {magic}, script)

    def test_a_changed_or_missing_archive_stops_the_release(self):
        packages = self.packages()
        tampered = packages / "sshound-1.2.3-aarch64-apple-darwin.tar.gz"
        tampered.write_bytes(b"something else")
        with self.assertRaises(ValueError):
            release.prepare("v1.2.3", "1.2.3", "someone/sshound", packages, self.root / "out", self.changelog)
        tampered.unlink()
        with self.assertRaises(OSError):
            release.prepare("v1.2.3", "1.2.3", "someone/sshound", packages, self.root / "out", self.changelog)


if __name__ == "__main__":
    unittest.main()
