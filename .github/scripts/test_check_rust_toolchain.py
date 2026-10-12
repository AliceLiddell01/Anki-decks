"""Интеграционные тесты проверяющего скрипта с изменёнными TOML, метаданными и выводом команд."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).with_name("check_rust_toolchain.py")

ROOT_CARGO = """[workspace]
members = ["tools/one", "tools/two", "tools/three"]
resolver = "3"
[workspace.package]
rust-version = "1.88.0"
edition = "2024"
"""
PIN = """[toolchain]
channel = "1.88.0"
profile = "minimal"
components = ["rustfmt", "clippy"]
"""
MEMBER = """[package]
name = "example"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
"""


class ToolchainContractTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write("Cargo.toml", ROOT_CARGO)
        self.write("rust-toolchain.toml", PIN)
        self.write("rustc-version.txt", "rustc 1.88.0 (test 2025-01-01)\nrelease: 1.88.0\n")
        self.write("cargo-version.txt", "cargo 1.88.0 (test 2025-01-01)\n")
        packages = []
        members = []
        for name in ("one", "two", "three"):
            path = f"tools/{name}/Cargo.toml"
            self.write(path, MEMBER.replace('name = "example"', f'name = "{name}"'))
            member_id = f"{name} 0.1.0 (path+file:///{name})"
            members.append(member_id)
            packages.append({
                "id": member_id,
                "name": name,
                "manifest_path": str(self.root / path),
                "rust_version": "1.88.0",
            })
        self.metadata = {"packages": packages, "workspace_members": members}
        self.save_metadata()

        bindir = self.root / "bin"
        bindir.mkdir()
        for name in ("cargo", "rustc"):
            executable = bindir / name
            executable.write_text(
                "#!/usr/bin/env python3\n"
                "import pathlib, sys\n"
                "root = pathlib.Path(__file__).resolve().parent.parent\n"
                f"if {name!r} == 'cargo' and len(sys.argv) > 1 and sys.argv[1] == 'metadata':\n"
                "    print((root / 'metadata.json').read_text()); sys.exit(0)\n"
                f"print((root / '{name}-version.txt').read_text(), end='')\n",
                encoding="utf-8",
            )
            executable.chmod(0o755)
        self.env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ.get("PATH", ""))

    def write(self, name, contents):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")

    def save_metadata(self):
        self.write("metadata.json", json.dumps(self.metadata))

    def invoke(self, expected=None):
        result = subprocess.run(
            [sys.executable, str(CHECKER), "--root", str(self.root)],
            text=True, capture_output=True, env=self.env,
        )
        if expected is None:
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Проверены все участники рабочей области Cargo: 3", result.stdout)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn(expected, result.stderr)

    def test_valid_contract(self):
        self.invoke()

    def test_mismatched_pin(self):
        self.write("rust-toolchain.toml", PIN.replace("1.88.0", "1.89.0"))
        self.invoke("различаются")

    def test_invalid_toolchain_toml(self):
        self.write("rust-toolchain.toml", "[toolchain\nchannel = '1.88.0'")
        self.invoke("корректный TOML")

    def test_missing_pin(self):
        self.write("rust-toolchain.toml", PIN.replace('channel = "1.88.0"\n', ""))
        self.invoke("требуется точная версия")

    def test_unversioned_channel_rejected(self):
        self.write("rust-toolchain.toml", PIN.replace("1.88.0", "stable"))
        self.invoke("требуется точная версия")

    def test_invalid_workspace_toml(self):
        self.write("Cargo.toml", "[workspace\nrust-version = '1.88.0'")
        self.invoke("корректный TOML")

    def test_incomplete_workspace_msrv(self):
        self.write("Cargo.toml", ROOT_CARGO.replace("1.88.0", "1.88"))
        self.invoke("требуется точная версия")

    def test_conflicting_member_override(self):
        self.write("tools/two/Cargo.toml", MEMBER.replace("rust-version.workspace = true", 'rust-version = "1.89.0"'))
        self.invoke("rust-version.workspace")

    def test_metadata_member_mismatch(self):
        self.metadata["packages"][2]["rust_version"] = "1.89.0"
        self.save_metadata()
        self.invoke("объявляет")

    def test_metadata_member_not_resolved(self):
        self.metadata["packages"].pop()
        self.save_metadata()
        self.invoke("не найден пакет")

    def test_wrong_rustc_with_correct_metadata(self):
        self.write("rustc-version.txt", "rustc 1.89.0 (test)\nrelease: 1.89.0\n")
        self.invoke("Фактический rustc")

    def test_wrong_cargo_with_correct_rustc(self):
        self.write("cargo-version.txt", "cargo 1.89.0 (test)\n")
        self.invoke("Фактический cargo")

    def test_legacy_override_must_not_shadow_pin(self):
        self.write("rust-toolchain", "stable\n")
        self.invoke("перекрывает rust-toolchain.toml")

    def test_missing_components(self):
        self.write("rust-toolchain.toml", PIN.replace('"clippy"', '"rust-docs"'))
        self.invoke("components")

    def test_missing_minimal_profile(self):
        self.write("rust-toolchain.toml", PIN.replace('"minimal"', '"default"'))
        self.invoke("profile")


if __name__ == "__main__":
    unittest.main()
