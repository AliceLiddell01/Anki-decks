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


def check_expected(package_files: dict, *, expected_current: str, expect_regex: bool = True, require_updates: bool = False) -> None:
    native = select(package_files, "rust-toolchain")
    custom = select(package_files, "regex")
    assert len(native) == 1, f"Ожидали один rust-toolchain, получено {native!r}"
    assert native[0][0] == "rust-toolchain.toml", native
    assert len(custom) == int(expect_regex), f"Неверное число custom.regex: {custom!r}"
    entries = native + custom
    for path, dep in entries:
        assert dep["depName"] == "rust", (path, dep)
        assert dep["currentValue"] == expected_current, (path, dep)
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
    current = tomllib.loads(cargo)["workspace"]["package"]["rust-version"]
    assert isinstance(current, str) and PIN.fullmatch(current), "MSRV не является точным pin X.Y.Z"
    assert tomllib.loads(toolchain)["toolchain"]["channel"] == current, "Rust toolchain и MSRV различаются"
    config = json.loads((ROOT / "renovate.json").read_text(encoding="utf-8"))
    assert config["automerge"] is False
    assert config["prConcurrentLimit"] == 4 and config["prHourlyLimit"] == 2
    assert config["commitHourlyLimit"] == 2
    full = run_renovate(ROOT, mode="lookup", managers=["cargo", "rust-toolchain", "custom.regex"])
    check_expected(full, expected_current=current)
    assert "cargo" in full and any(p.get("deps") for p in full["cargo"]), "Cargo-зависимости не извлечены"

    with tempfile.TemporaryDirectory(prefix="renovate-rust-test-") as tmp:
        directory = Path(tmp)
        shutil.copy(ROOT / "renovate.json", directory)
        shutil.copy(ROOT / "rust-toolchain.toml", directory)
        # Историческая версия — только в изолированном сценарии, не в production CI.
        historical = "1.88.0"
        fixture_cargo = cargo.replace(f'rust-version = "{current}"', f'rust-version = "{historical}"', 1)
        fixture_toolchain = toolchain.replace(f'channel = "{current}"', f'channel = "{historical}"', 1)
        assert fixture_cargo != cargo or current == historical
        assert fixture_toolchain != toolchain or current == historical
        (directory / "Cargo.toml").write_text(fixture_cargo, encoding="utf-8")
        (directory / "rust-toolchain.toml").write_text(fixture_toolchain, encoding="utf-8")
        historical_result = run_renovate(
            directory, mode="lookup", managers=["rust-toolchain", "custom.regex"]
        )
        check_expected(historical_result, expected_current=historical, require_updates=True)
        historical_custom = select(historical_result, "regex")
        update_value = historical_custom[0][1]["updates"][0]["newValue"]

        commented = fixture_cargo.replace(
            f'rust-version = "{historical}"',
            f'# MSRV {historical}\nrust-version = "{historical}"',
            1,
        )
        (directory / "Cargo.toml").write_text(commented, encoding="utf-8")
        comment_result = run_renovate(
            directory, mode="extract", managers=["rust-toolchain", "custom.regex"]
        )
        check_expected(comment_result, expected_current=historical)
        comment_dep = select(comment_result, "regex")[0][1]
        replace_string = comment_dep["replaceString"]
        assert f"# MSRV {historical}" not in replace_string, replace_string
        assert f'rust-version = "{historical}"' in replace_string, replace_string
        replacement = replace_string.replace(historical, update_value, 1)
        updated_comment_fixture = commented.replace(replace_string, replacement, 1)
        assert f"# MSRV {historical}" in updated_comment_fixture
        assert f'rust-version = "{update_value}"' in updated_comment_fixture
        assert f"# MSRV {update_value}" not in updated_comment_fixture

        multiline = fixture_cargo.replace(
            f'rust-version = "{historical}"',
            f'keywords = [\n  "cli",\n]\nrust-version = "{historical}"',
            1,
        )
        assert tomllib.loads(multiline)["workspace"]["package"]["keywords"] == ["cli"]
        (directory / "Cargo.toml").write_text(multiline, encoding="utf-8")
        multiline_result = run_renovate(
            directory, mode="extract", managers=["rust-toolchain", "custom.regex"]
        )
        check_expected(multiline_result, expected_current=historical)

        (directory / "Cargo.toml").write_text(
            fixture_cargo + '\n[workspace.metadata]\nrust-version = "9.9.9"\n',
            encoding="utf-8",
        )
        assert tomllib.loads((directory / "Cargo.toml").read_text())["workspace"]["metadata"]["rust-version"] == "9.9.9"
        check_expected(run_renovate(directory, mode="extract", managers=["rust-toolchain", "custom.regex"]), expected_current=historical)
        (directory / "Cargo.toml").write_text(
            fixture_cargo.replace(f'rust-version = "{historical}"', f'minimum-rust-version = "{historical}"', 1),
            encoding="utf-8",
        )
        check_expected(run_renovate(directory, mode="extract", managers=["rust-toolchain", "custom.regex"]), expected_current=historical, expect_regex=False)
        doubled = fixture_cargo.replace(f'rust-version = "{historical}"', f'rust-version = "{historical}"\nrust-version = "1.89.0"', 1)
        try:
            tomllib.loads(doubled)
        except tomllib.TOMLDecodeError:
            pass
        else:
            raise AssertionError("Повторяющийся workspace MSRV ошибочно принят как корректный TOML")
    print("Renovate: локальная изоляция, отсутствие MSRV и дублирующиеся ключи проверены")


if __name__ == "__main__":
    main()
