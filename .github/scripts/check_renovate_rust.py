#!/usr/bin/env python3
"""Проверяет извлечение и группировку Rust настоящим движком Renovate."""

import json
import os
import re
import shutil
import subprocess
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PIN = re.compile(r"\d+\.\d+\.\d+\Z")


def run_renovate(directory: Path, *, mode: str, managers: list[str]) -> dict:
    env = dict(os.environ)
    env.update({
        "LOG_LEVEL": "trace",
        "LOG_FORMAT": "json",
        "RENOVATE_PLATFORM": "local",
        "RENOVATE_ENABLED_MANAGERS": json.dumps(managers),
    })
    result = subprocess.run(
        [str(ROOT / "node_modules/.bin/renovate"), "--platform=local", f"--dry-run={mode}"],
        cwd=directory, env=env, capture_output=True, text=True, timeout=600,
    )
    if result.returncode:
        raise AssertionError(
            f"Renovate ({mode}) завершился с кодом {result.returncode}:\n"
            f"{(result.stderr + result.stdout)[-10000:]}"
        )
    events = []
    for line in (result.stdout + "\n" + result.stderr).splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            events.append(event)
    for target in ("packageFiles with updates", "packageFiles", "Extracted dependencies"):
        candidates = [event for event in events if event.get("msg") == target]
        for event in reversed(candidates):
            package_files = event.get("config") if target != "Extracted dependencies" else event.get("packageFiles")
            if isinstance(package_files, dict):
                return package_files
    messages = [e.get("msg") for e in events if e.get("msg")]
    raise AssertionError(f"Renovate не опубликовал результаты извлечения: {messages[-30:]}")


def select(package_files: dict, manager: str) -> list[tuple[str, dict]]:
    found = []
    for name, packages in package_files.items():
        if manager == "regex" and name not in ("regex", "custom.regex"):
            continue
        if manager != "regex" and name != manager:
            continue
        for package in packages:
            for dep in package["deps"]:
                if dep.get("datasource") == "rust-version":
                    found.append((package["packageFile"], dep))
    return found


def check_expected(package_files: dict, *, expect_regex: bool = True, require_updates: bool = False) -> None:
    native = select(package_files, "rust-toolchain")
    custom = select(package_files, "regex")
    assert len(native) == 1, f"Ожидали один rust-toolchain, получено {native!r}"
    assert native[0][0] == "rust-toolchain.toml", native
    assert len(custom) == int(expect_regex), f"Неверное число custom.regex: {custom!r}"
    entries = native + custom
    for path, dep in entries:
        assert dep["depName"] == "rust", (path, dep)
        assert dep["currentValue"] == "1.88.0", (path, dep)
        assert dep.get("skipReason") is None, (path, dep)
    if not require_updates:
        return
    assert len(custom) == 1, "Для проверки группы необходимы оба manager"
    updates = [
        (path, update)
        for path, dep in entries
        for update in dep.get("updates", [])
        if update.get("updateType") in ("major", "minor", "patch")
    ]
    by_file = {}
    for path, update in updates:
        by_file.setdefault(path, []).append(update)
    assert set(by_file) == {"Cargo.toml", "rust-toolchain.toml"}, by_file
    for path, candidates in by_file.items():
        assert len(candidates) == 1, (path, candidates)
    a, b = (by_file[path][0] for path in ("Cargo.toml", "rust-toolchain.toml"))
    for name, update in (("Cargo.toml", a), ("rust-toolchain.toml", b)):
        assert PIN.fullmatch(update["newValue"]), (name, update)
        assert PIN.fullmatch(update["newVersion"]), (name, update)
    assert a["newValue"] == b["newValue"], (a, b)
    assert a["branchName"] == b["branchName"], (a, b)
    print(f"Renovate: обе записи обновятся до {a['newValue']} в ветке {a['branchName']}")


def main():
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    toolchain = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
    assert tomllib.loads(cargo)["workspace"]["package"]["rust-version"] == "1.88.0"
    assert tomllib.loads(toolchain)["toolchain"]["channel"] == "1.88.0"
    config = json.loads((ROOT / "renovate.json").read_text(encoding="utf-8"))
    assert config["automerge"] is False
    assert config["prConcurrentLimit"] == 4 and config["prHourlyLimit"] == 2
    assert config["commitHourlyLimit"] == 2
    full = run_renovate(ROOT, mode="lookup", managers=["cargo", "rust-toolchain", "custom.regex"])
    check_expected(full, require_updates=True)
    assert "cargo" in full and any(p.get("deps") for p in full["cargo"]), "Cargo-зависимости не извлечены"

    with tempfile.TemporaryDirectory(prefix="renovate-rust-test-") as tmp:
        directory = Path(tmp)
        shutil.copy(ROOT / "renovate.json", directory)
        shutil.copy(ROOT / "rust-toolchain.toml", directory)
        (directory / "Cargo.toml").write_text(
            cargo + '\n[workspace.metadata]\nrust-version = "9.9.9"\n',
            encoding="utf-8",
        )
        assert tomllib.loads((directory / "Cargo.toml").read_text())["workspace"]["metadata"]["rust-version"] == "9.9.9"
        check_expected(run_renovate(directory, mode="extract", managers=["rust-toolchain", "custom.regex"]))
        (directory / "Cargo.toml").write_text(cargo.replace("rust-version = \"1.88.0\"", "minimum-rust-version = \"1.88.0\""), encoding="utf-8")
        check_expected(run_renovate(directory, mode="extract", managers=["rust-toolchain", "custom.regex"]), expect_regex=False)
        doubled = cargo.replace('rust-version = "1.88.0"', 'rust-version = "1.88.0"\nrust-version = "1.89.0"')
        try:
            tomllib.loads(doubled)
        except tomllib.TOMLDecodeError:
            pass
        else:
            raise AssertionError("Повторяющийся workspace MSRV ошибочно принят как корректный TOML")
    print("Renovate: локальная изоляция, отсутствие MSRV и дублирующиеся ключи проверены")


if __name__ == "__main__":
    main()
