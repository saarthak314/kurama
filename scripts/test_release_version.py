"""Regression tests for release metadata; no Rust toolchain required."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHECK = ROOT / "scripts/check-release-version.py"


class ReleaseVersionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "cli").mkdir()
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["cli"]\n'
            '[workspace.package]\nversion = "0.1.10"\n'
        )
        (self.root / "cli/Cargo.toml").write_text(
            '[package]\nname = "kurama-cli"\nversion.workspace = true\n'
        )
        (self.root / "Cargo.lock").write_text(
            'version = 4\n[[package]]\nname = "kurama-cli"\n'
            'version = "0.1.10"\n[[package]]\nname = "dependency"\n'
            'version = "0.1.9"\nsource = "registry+https://example.com"\n'
        )

    def check(self, *args):
        return subprocess.run(
            [sys.executable, str(CHECK), "--root", str(self.root), *args],
            capture_output=True,
            text=True,
        )

    def test_matching_tag_accepts_unchanged_dependency_version(self):
        result = self.check("v0.1.10")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v0.1.10", result.stdout)

    def test_stale_workspace_lock_version_is_rejected(self):
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('version = "0.1.10"', 'version = "0.1.9"'))
        result = self.check("v0.1.10")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("kurama-cli", result.stderr)

    def set_version(self, version, old="0.1.10"):
        for filename in ("Cargo.toml", "Cargo.lock"):
            path = self.root / filename
            path.write_text(path.read_text().replace(old, version))

    def test_four_component_version_is_rejected_even_when_metadata_matches(self):
        self.set_version("0.1.9.5")
        result = self.check("v0.1.9.5")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SemVer", result.stderr)

    def make_binary(self, output, status=0):
        binary = self.root / "kurama"
        binary.write_text(
            f"#!{sys.executable}\nimport sys\n"
            "assert sys.argv[1:] == ['--version']\n"
            f"print({output!r})\nsys.exit({status})\n"
        )
        binary.chmod(0o755)
        return str(binary)

    def test_runs_binary_version_before_accepting_release(self):
        binary = self.make_binary("kurama 0.1.10")
        result = self.check("v0.1.10", "--binary", binary)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_ci_and_release_run_validation_regressions(self):
        command = "python3 -m unittest discover -s scripts -p 'test_release_version.py' -v"
        for workflow in ("ci.yml", "release.yml"):
            with self.subTest(workflow=workflow):
                text = (ROOT / ".github/workflows" / workflow).read_text()
                self.assertIn(command, text)
                self.assertIn("python3 scripts/check-release-version.py", text)

    def test_release_checks_tag_before_tests_and_binary_before_archive(self):
        text = (ROOT / ".github/workflows/release.yml").read_text()
        source_guard = 'python3 scripts/check-release-version.py "$GITHUB_REF_NAME"'
        binary_guard = source_guard + ' --binary target/${{ matrix.target }}/release/kurama'
        self.assertIn(source_guard, text)
        self.assertIn(binary_guard, text)
        self.assertLess(text.index(source_guard), text.index("cargo test --locked"))
        self.assertLess(text.index("cargo build --locked"), text.index(binary_guard))
        self.assertLess(text.index("scripts/check-size.sh"), text.index(binary_guard))
        self.assertLess(text.index(binary_guard), text.index("Package archive and checksum"))

    def test_tag_must_match_exactly(self):
        for tag in ("v0.1.9", "v0.1.9.5", "v0.1.10.1", "0.1.10", "V0.1.10",
                    "vv0.1.10", "v0.1.10-rc.1", "v0.1.10+build", "v0.1.10\n", ""):
            with self.subTest(tag=tag):
                result = self.check(tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("exactly 'v0.1.10'", result.stderr)

    def test_semver_edge_cases(self):
        cases = {
            "0.0.0": True, "10.20.30": True, "1.2.3-rc.1": True,
            "1.2.3-0+build.01": True, "1.2.3-alpha-beta.2": True,
            "01.2.3": False, "1.02.3": False, "1.2.03": False,
            "1.2": False, "1.2.3.4": False, "1.2.3-01": False,
            "1.2.3-": False, "1.2.3+": False, "1.2.3-rc..1": False,
            "1.2.3+build..1": False, "1.2.3_rc1": False,
        }
        for version, valid in cases.items():
            with self.subTest(version=version):
                self.set_version(version)
                result = self.check(f"v{version}")
                self.assertEqual(result.returncode == 0, valid, result.stderr)
                self.set_version("0.1.10", old=version)

    def test_missing_duplicate_or_registry_only_member_is_rejected(self):
        lock = self.root / "Cargo.lock"
        entry = '[[package]]\nname = "kurama-cli"\nversion = "0.1.10"\n'
        for contents in (
            'version = 4\npackage = []\n',
            'version = 4\n' + entry * 2,
            'version = 4\n' + entry + 'source = "registry+https://example.com"\n',
        ):
            with self.subTest(lock=contents):
                lock.write_text(contents)
                result = self.check()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("kurama-cli", result.stderr)

    def test_member_must_inherit_workspace_version(self):
        manifest = self.root / "cli/Cargo.toml"
        manifest.write_text('[package]\nname = "kurama-cli"\nversion = "0.1.9"\n')
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must inherit", result.stderr)

    def test_missing_or_malformed_lockfile_is_rejected(self):
        lock = self.root / "Cargo.lock"
        lock.unlink()
        self.assertNotEqual(self.check().returncode, 0)
        lock.write_text("not valid TOML")
        self.assertNotEqual(self.check().returncode, 0)

    def test_source_only_check(self):
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_actual_workspace_is_consistent(self):
        result = subprocess.run(
            [sys.executable, str(CHECK)], capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_wrong_binary_version_is_rejected(self):
        for output in ("kurama 0.1.9", "kurama v0.1.10", "0.1.10",
                       "kurama 0.1.10 extra", "kurama 0.1.10\nextra"):
            with self.subTest(output=output):
                result = self.check("v0.1.10", "--binary", self.make_binary(output))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("binary --version", result.stderr)

    def test_failed_or_missing_binary_is_rejected(self):
        binary = self.make_binary("kurama 0.1.10", status=1)
        self.assertNotEqual(self.check("v0.1.10", "--binary", binary).returncode, 0)
        Path(binary).unlink()
        self.assertNotEqual(self.check("v0.1.10", "--binary", binary).returncode, 0)


if __name__ == "__main__":
    unittest.main()
