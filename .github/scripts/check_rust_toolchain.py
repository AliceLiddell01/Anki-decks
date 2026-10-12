#!/usr/bin/env python3
"""Останавливает CI при рассинхронизации MSRV, rustup и фактической версии Rust."""

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

RELEASE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


class ContractError(Exception):
    """Нарушение контракта версии Rust в репозитории."""


def release(value, where):
    if not isinstance(value, str) or not RELEASE.fullmatch(value):
        raise ContractError(f"{where}: требуется точная версия X.Y.Z, получено {value!r}")
    return tuple(int(part) for part in value.split("."))


def read_toml(path):
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        raise ContractError(f"Не удалось прочитать корректный TOML {path}: {exc}") from exc


def table(parent, key, where):
    value = parent.get(key) if isinstance(parent, dict) else None
    if not isinstance(value, dict):
        raise ContractError(f"{where}: отсутствует таблица [{key}]")
    return value


def run(root, *args):
    try:
        result = subprocess.run(args, cwd=root, capture_output=True, text=True, check=False)
    except OSError as exc:
        raise ContractError(f"Не удалось запустить {args[0]}: {exc}") from exc
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ContractError(f"Команда {' '.join(args)} завершилась с кодом {result.returncode}: {detail}")
    return result.stdout


def check(root):
    if (root / "rust-toolchain").exists():
        raise ContractError("Файл rust-toolchain перекрывает rust-toolchain.toml по правилам rustup")

    cargo_doc = read_toml(root / "Cargo.toml")
    workspace = table(cargo_doc, "workspace", "Cargo.toml")
    package_defaults = table(workspace, "package", "Cargo.toml [workspace]")
    msrv = release(package_defaults.get("rust-version"), "Cargo.toml [workspace.package].rust-version")

    toolchain_doc = read_toml(root / "rust-toolchain.toml")
    pin = table(toolchain_doc, "toolchain", "rust-toolchain.toml")
    pinned = release(pin.get("channel"), "rust-toolchain.toml [toolchain].channel")
    if pinned != msrv:
        raise ContractError(f"MSRV в Cargo.toml и pin в rust-toolchain.toml различаются: {msrv} != {pinned}")
    if pin.get("profile") != "minimal":
        raise ContractError("rust-toolchain.toml: ожидается profile = 'minimal'")
    components = pin.get("components")
    if (
        not isinstance(components, list)
        or not all(isinstance(component, str) for component in components)
        or sorted(components) != ["clippy", "rustfmt"]
    ):
        raise ContractError("rust-toolchain.toml: components должны содержать rustfmt и clippy")
    if "path" in pin:
        raise ContractError("rust-toolchain.toml: path не допускается вместе с закреплённым channel")

    try:
        metadata = json.loads(run(root, "cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"))
    except (ValueError, TypeError) as exc:
        raise ContractError(f"cargo metadata вернул некорректный JSON: {exc}") from exc
    if not isinstance(metadata, dict):
        raise ContractError("cargo metadata: ожидался JSON-объект")
    members = metadata.get("workspace_members")
    packages = metadata.get("packages")
    if not isinstance(members, list) or not members or not isinstance(packages, list):
        raise ContractError("cargo metadata: отсутствуют участники или packages")
    if len(members) != len(set(members)):
        raise ContractError("cargo metadata: повторяющиеся workspace_members")
    by_id = {p["id"]: p for p in packages if isinstance(p, dict) and isinstance(p.get("id"), str)}
    for member_id in members:
        if not isinstance(member_id, str) or member_id not in by_id:
            raise ContractError(f"cargo metadata: не найден пакет участника {member_id!r}")
        member = by_id[member_id]
        path_value = member.get("manifest_path")
        if not isinstance(path_value, str):
            raise ContractError(f"cargo metadata: у {member_id} отсутствует manifest_path")
        member_manifest = Path(path_value)
        if not member_manifest.is_absolute():
            raise ContractError(f"cargo metadata: manifest_path не абсолютный: {path_value}")
        manifest = read_toml(member_manifest)
        package = table(manifest, "package", str(member_manifest))
        if package.get("rust-version") != {"workspace": True}:
            raise ContractError(f"{member_manifest}: требуется rust-version.workspace = true")
        actual = release(member.get("rust_version"), f"cargo metadata: {member_id}.rust_version")
        if actual != msrv:
            raise ContractError(f"cargo metadata: {member_id} объявляет {actual}, ожидается {msrv}")

    expected = ".".join(str(part) for part in msrv)
    rustc = run(root, "rustc", "--version", "--verbose")
    found = re.search(r"^release:\s*(\S+)\s*$", rustc, re.MULTILINE)
    if not found or release(found.group(1), "rustc --version --verbose") != msrv:
        raise ContractError(f"Фактический rustc не соответствует {expected}: {rustc.strip()}")
    cargo = run(root, "cargo", "--version")
    found = re.match(r"^cargo\s+(\S+)", cargo)
    if not found or release(found.group(1), "cargo --version") != msrv:
        raise ContractError(f"Фактический cargo не соответствует {expected}: {cargo.strip()}")

    print(f"Контракт Rust: MSRV = версия rustup = rustc = cargo = {expected}")
    print(f"Проверены все участники рабочей области Cargo: {len(members)}")
    print(rustc.strip())
    print(cargo.strip())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    try:
        check(args.root.resolve())
    except (ContractError, KeyError, TypeError) as exc:
        print(f"::error::Нарушен контракт Rust toolchain: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
