#!/usr/bin/env python3
"""Локальная live-приёмка Yarxi; все свидетельства сохраняются, сеть запускает CLI.

Пример: python3 tools/asset-store/scripts/yarxi_parallel_acceptance.py benchmark \
    --repository-root . --binary target/debug/kanji-assets --corpus <список.txt>

Smoke принимает --rotate-after и --interrupt-after для отдельной сборки с
session-rotation-acceptance. Benchmark последовательно запускает 1/2/3/4 workers
на первых 300 identities; soak запускает один процесс на первых 1000 identities.
Порядок корпуса сохраняется. Подготовка корпуса и выбор production default
принадлежат вызывающему агенту; runner их не подменяет.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import selectors
import signal
import stat
import subprocess
import sys
import time
import uuid


DEFAULT_REPORTS = Path(".codex/local/reports/yarxi-parallel-acceptance")
SAMPLE_INTERVAL = 1.0


class AcceptanceError(Exception):
    """Ошибка приёмки с сообщением для оператора."""


def write_json(path, value):
    # Файлы отчёта обновляются атомарно; исходные журналы всегда дописываются.
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def append_json(file, value):
    file.write(json.dumps(value, ensure_ascii=False) + "\n")
    file.flush()


def unavailable(reason):
    return {"available": False, "reason": reason}


def temporary_path_references_root(value, cwd, root):
    if not value:
        return False
    path = Path(value)
    if not path.is_absolute():
        if not cwd:
            return True
        path = Path(cwd) / path
    path = Path(os.path.abspath(path))
    root = Path(os.path.abspath(root))
    return path == root or root in path.parents


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def percentile(values):
    if not values:
        return unavailable("В журнале нет подтверждённых измерений этого типа")
    ordered = sorted(values)

    def quantile(fraction):
        position = (len(ordered) - 1) * fraction
        low, high = math.floor(position), math.ceil(position)
        return ordered[low] + (ordered[high] - ordered[low]) * (position - low)

    return {"available": True, "count": len(values), "p50_ms": quantile(0.5), "p95_ms": quantile(0.95)}


def git(repository, *arguments):
    result = subprocess.run(["git", "-C", str(repository), *arguments], capture_output=True)
    if result.returncode != 0:
        raise AcceptanceError("Git: " + result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout


def ignored_root(repository, requested):
    root = requested.resolve()
    permitted = (repository / DEFAULT_REPORTS).resolve()
    if not root.is_relative_to(permitted):
        raise AcceptanceError(f"Каталог отчётов должен находиться внутри {permitted}")
    root.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(
        ["git", "-C", str(repository), "check-ignore", "--no-index", str(root)], capture_output=True
    )
    if result.returncode != 0:
        raise AcceptanceError(f"Git не подтверждает игнорирование каталога {root}")
    # Один корень нужен для сравнительных прогонов; файл блокировки исключает
    # конкурентный запуск двух приёмок с собственными browser workers.
    return root


def file_state(path):
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return {"kind": "missing"}
    result = {"mode": stat.S_IMODE(metadata.st_mode)}
    if stat.S_ISLNK(metadata.st_mode):
        result.update(kind="symlink", target=os.readlink(path))
    elif stat.S_ISREG(metadata.st_mode):
        result.update(kind="file", bytes=metadata.st_size, sha256=digest(path))
    elif stat.S_ISDIR(metadata.st_mode):
        result.update(kind="directory")
    else:
        result.update(kind="special")
    return result


def protected_state(repository):
    tracked = git(repository, "ls-files", "-z").decode("utf-8", errors="surrogateescape").split("\0")
    paths = {name for name in tracked if name}
    # Незакоммиченные файлы внутри decks и исходного canonical store тоже защищены.
    for relative in ("decks", ".asset-store"):
        directory = repository / relative
        if directory.exists() or directory.is_symlink():
            paths.add(relative)
            if not directory.is_symlink():
                paths.update(str(path.relative_to(repository)) for path in directory.rglob("*"))
    return {
        "index_sha256": hashlib.sha256(git(repository, "ls-files", "--stage", "-z")).hexdigest(),
        "paths": {name: file_state(repository / name) for name in sorted(paths)},
    }


def protection_diff(before, after):
    previous, current = before["paths"], after["paths"]
    return {
        "unchanged": before == after,
        "index_changed": before["index_sha256"] != after["index_sha256"],
        "changed_paths": [name for name in sorted(previous.keys() | current.keys()) if previous.get(name) != current.get(name)],
    }


def verify_protection(repository, baseline, output):
    comparison = protection_diff(baseline, protected_state(repository))
    write_json(output, comparison)
    if not comparison["unchanged"]:
        raise AcceptanceError(f"Изменились защищённые файлы; см. {output}")
    return comparison


def corpus_input(path, expected_sha, needed):
    raw = path.read_bytes()
    sha = hashlib.sha256(raw).hexdigest()
    if expected_sha and sha != expected_sha.lower():
        raise AcceptanceError(f"SHA-256 корпуса не совпал: получен {sha}")
    try:
        values = raw.decode("utf-8").splitlines()
    except UnicodeDecodeError as error:
        raise AcceptanceError("Корпус должен быть UTF-8") from error
    invalid = [index + 1 for index, value in enumerate(values) if len(value) != 1 or value.isspace()]
    if invalid:
        raise AcceptanceError(f"Нужен один точный символ в каждой строке; нарушены строки {invalid[:10]}")
    if len(set(values)) != len(values):
        raise AcceptanceError("Корпус содержит повторяющиеся identities")
    if len(values) < needed:
        raise AcceptanceError(f"Недостаточно уникальных identities: {len(values)}, требуется {needed}")
    # Актуальный domain validator применяется самим batch start; диапазоны Unicode
    # здесь не копируются. Проверяем SHA и сохраняем полный исходный файл без правки.
    return raw, values, sha


def identity(record):
    value = record.get("identity")
    if isinstance(value, dict):
        return value.get("key")
    return value if isinstance(value, str) else None


def process_table():
    """Снимок /proc; cmdline/environ используются только для доказательства владения."""
    if not Path("/proc/self/stat").exists():
        return {}, unavailable("Linux /proc недоступен")
    rows, denied, environment_denied, cwd_denied, zombies = {}, 0, 0, 0, 0
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            if path.stat().st_uid != os.getuid():
                continue
            raw_stat = (path / "stat").read_text()
            fields = raw_stat[raw_stat.rfind(")") + 2 :].split()
            if fields[0] == "Z":
                # Zombies have no live descriptors or browser/workspace resources.
                zombies += 1
                continue
            command = (path / "cmdline").read_bytes().split(b"\0")
            executable = os.path.basename(command[0].decode(errors="replace")) if command else ""
            process_cwd = None
            if "chrom" in executable.lower():
                try:
                    process_cwd = os.readlink(path / "cwd")
                except PermissionError:
                    cwd_denied += 1
            try:
                environment = (path / "environ").read_bytes().split(b"\0")
            except PermissionError:
                # cmdline and ancestry identify owned Chromium; environ is only
                # an additional TMPDIR signal and may be restricted for services.
                environment = []
                environment_denied += 1
            temporary = next((entry[7:].decode(errors="replace") for entry in environment if entry.startswith(b"TMPDIR=")), None)
            rows[int(path.name)] = {
                "pid": int(path.name), "ppid": int(fields[1]), "start_ticks": int(fields[19]),
                "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
                "chromium": "chrom" in executable.lower(), "cwd": process_cwd,
                "references": [entry.decode(errors="replace") for entry in command], "tmpdir": temporary,
            }
        except (FileNotFoundError, ProcessLookupError):
            continue
        except PermissionError:
            denied += 1
        except (OSError, ValueError, IndexError):
            denied += 1
    return rows, {
        "available": True,
        "unreadable_same_uid_processes": denied,
        "unreadable_environments": environment_denied,
        "unreadable_process_cwds": cwd_denied,
        "ignored_zombie_processes": zombies,
    }


class ResourceObserver:
    def __init__(self, process, run_root):
        self.pid = process.pid
        self.root = run_root
        self.known = set()
        self.workspaces = set()
        self.peak_rss = 0
        self.peak_processes = 0
        self.samples = 0
        self.observation = unavailable("Снимки процессов ещё не сняты")

    def snapshot(self, elapsed):
        rows, observation = process_table()
        self.observation = observation
        owned = {pid for pid, row in rows.items() if (pid, row["start_ticks"]) in self.known}
        owned.add(self.pid)
        while True:
            children = {pid for pid, row in rows.items() if row["ppid"] in owned}
            if children <= owned:
                break
            owned |= children
        browser = []
        for pid, row in rows.items():
            root_reference = any(str(self.root) in reference for reference in row["references"])
            root_reference |= temporary_path_references_root(row["tmpdir"], row["cwd"], self.root)
            if row["chromium"] and (pid in owned or root_reference):
                self.known.add((pid, row["start_ticks"]))
                browser.append({key: row[key] for key in ("pid", "start_ticks", "rss_bytes", "tmpdir")})
        # Маркер подтверждает ownership workspace конкретным PID CLI. Проверяем
        # также прежний /tmp namespace, чтобы нарушение размещения было видно.
        for parent in (self.root / "runtime-workspaces", Path("/tmp")):
            namespace = parent / f"anki-decks-{os.getuid()}"
            if not namespace.is_dir():
                continue
            for marker in namespace.glob("run-*/.anki-decks-owner.json"):
                try:
                    owner = json.loads(marker.read_text())
                    if owner.get("pid") == self.pid:
                        self.workspaces.add(str(marker.parent))
                except (OSError, ValueError):
                    continue
        rss = sum(row["rss_bytes"] for row in browser)
        self.peak_rss = max(self.peak_rss, rss)
        self.peak_processes = max(self.peak_processes, len(browser))
        self.samples += 1
        return {"elapsed_seconds": elapsed, "browser_rss_bytes": rss, "chromium_processes": len(browser), "processes": browser, "observation": observation}

    def finish(self):
        final = self.snapshot(0)
        remaining = [path for path in sorted(self.workspaces) if Path(path).exists()]
        outside = [path for path in sorted(self.workspaces) if not Path(path).is_relative_to(self.root)]
        observed = self.observation.get("available", False)
        return {
            "peak_sampled_browser_rss_bytes": self.peak_rss if observed else unavailable("/proc недоступен"),
            "peak_sampled_chromium_processes": self.peak_processes if observed else unavailable("/proc недоступен"),
            "sampling_interval_seconds": SAMPLE_INTERVAL,
            "sample_count": self.samples,
            "scope": "Наблюдаемые Chromium потомки CLI и процессы с ссылками на run root; RSS суммируется, общие страницы могут учитываться повторно",
            "peak_limit": "Выборочный максимум является нижней границей; короткоживущие процессы могут отсутствовать между снимками",
            "orphan_owned_chromium": final["processes"] if observed else unavailable("/proc недоступен"),
            "remaining_owned_workspaces": remaining,
            "owned_workspaces_outside_reports": outside,
            "inspection": self.observation,
            "cleanup_proven": observed and self.observation.get("unreadable_same_uid_processes") == 0 and not final["processes"] and not remaining,
            "observation_limit": "Только обнаруженные по ownership markers и PID ресурсы; отсутствие неизвестных короткоживущих workspace не доказано",
        }


class Progress:
    def __init__(self, directory, requested):
        self.directory = directory
        self.requested = set(requested)
        self.starts = {}
        self.checkpoints = []
        self.events = Counter()
        self.session_end_reasons = Counter()
        self.outcomes = Counter()
        self.duplicates = []
        self.unexpected = []
        self.non_json_lines = 0
        self.last_elapsed = 0.0
        self.window_started = 0.0
        self.rolling = []
        self.event_file = (directory / "progress.jsonl").open("w", encoding="utf-8")
        self.rolling_file = (directory / "rolling.jsonl").open("w", encoding="utf-8")

    def consume(self, raw):
        try:
            value = json.loads(raw)
        except (ValueError, UnicodeDecodeError):
            self.non_json_lines += 1
            return
        if not isinstance(value, dict) or not isinstance(value.get("event"), str):
            return
        event = value["event"]
        self.events[event] += 1
        if event == "browser_session_ended":
            self.session_end_reasons[str(value.get("reason", "unavailable"))] += 1
        append_json(self.event_file, value)
        key = identity(value)
        elapsed = value.get("elapsed_ms")
        if not isinstance(elapsed, (int, float)):
            elapsed = None
        if event == "item_started" and key and elapsed is not None:
            self.starts[key] = elapsed
        if event != "item_checkpointed":
            return
        if key in {row["identity"] for row in self.checkpoints}:
            self.duplicates.append(key)
        if key not in self.requested:
            self.unexpected.append(key)
        reported_latency = value.get("item_elapsed_ms")
        latency = reported_latency if isinstance(reported_latency, (int, float)) else elapsed - self.starts[key] if elapsed is not None and key in self.starts else None
        self.outcomes[str(value.get("outcome", "unavailable"))] += 1
        self.checkpoints.append({
            "identity": key, "elapsed_ms": elapsed, "latency_ms": latency,
            "outcome": value.get("outcome"), "worker_id": value.get("worker_id"),
            "generation": value.get("generation"), "attempt": value.get("owner_attempt"),
            "reported_item_elapsed_ms": reported_latency,
            "stage_timings": value.get("stage_timings"),
        })
        if elapsed is not None:
            self.last_elapsed = elapsed / 1000
        if len(self.checkpoints) % 100 == 0:
            window = self.checkpoints[-100:]
            span = self.last_elapsed - self.window_started
            row = {
                "checkpointed": len(self.checkpoints), "window_items": 100,
                "window_seconds": span,
                "checkpoints_per_minute": 6000 / span if span > 0 else None,
                "item_latency": percentile([item["latency_ms"] for item in window if item["latency_ms"] is not None]),
                "outcomes": dict(Counter(item["outcome"] for item in window)),
                "cumulative_events": dict(self.events),
                "stage_latency": unavailable("Подробная диагностика читается после завершения процесса; см. итоговый rolling-windows.json"),
            }
            self.rolling.append(row)
            append_json(self.rolling_file, row)
            self.window_started = self.last_elapsed
            print(f"Сохранено {len(self.checkpoints)}/{len(self.requested)}; окно: {row['checkpoints_per_minute']:.1f} checkpoint/мин" if span > 0 else f"Сохранено {len(self.checkpoints)}", flush=True)

    def close(self):
        self.event_file.close()
        self.rolling_file.close()


def disk_usage(directory):
    logical, allocated, files = 0, 0, 0
    for path in directory.rglob("*"):
        metadata = path.lstat()
        if stat.S_ISREG(metadata.st_mode):
            logical += metadata.st_size
            allocated += metadata.st_blocks * 512
            files += 1
    return {"files": files, "logical_bytes": logical, "allocated_bytes": allocated}


def post_run_cleanup_audit(directory, previous_resources=None):
    rows, observation = process_table()
    root = directory.resolve()
    chromium = []
    for row in rows.values():
        if not row["chromium"]:
            continue
        references_root = any(str(root) in reference for reference in row["references"])
        references_root |= temporary_path_references_root(row["tmpdir"], row["cwd"], root)
        if references_root:
            chromium.append({key: row[key] for key in ("pid", "ppid", "start_ticks", "references", "tmpdir")})
    workspaces = []
    workspace_root = root / "runtime-workspaces"
    if workspace_root.exists():
        workspaces = [str(path) for path in sorted(workspace_root.rglob("run-*")) if path.is_dir()]
    outside = (previous_resources or {}).get("owned_workspaces_outside_reports", [])
    return {
        "available": observation.get("available", False),
        "process_inspection": observation,
        "chromium_processes_referencing_run_root": chromium,
        "remaining_run_workspaces": workspaces,
        "previously_observed_owned_workspaces_outside_reports": outside,
        "cleanup_proven": (
            observation.get("available", False)
            and observation.get("unreadable_same_uid_processes") == 0
            and not chromium and not workspaces and not outside
        ),
        "scope": "Повторный post-run /proc scan; command line and ancestry identify Chromium ownership, unreadable environ is reported separately, zombies are excluded because they hold no live resources.",
    }


def execute_command(command, directory, environment, repository, progress=None, interrupt_after=None):
    ignored_root(repository, directory.parent)
    directory.mkdir(exist_ok=False)
    process_cwd = Path(environment["ASSET_STORE_TEMP_ROOT"])
    write_json(directory / "command.json", {"argv": command, "cwd": str(process_cwd), "environment_overrides": {key: environment[key] for key in ("TMPDIR", "ASSET_STORE_TEMP_ROOT")}})
    start = time.monotonic()
    interrupted = False
    raw_buffer = b""
    with (directory / "stdout.txt").open("wb") as stdout, (directory / "stderr.txt").open("wb") as stderr, (directory / "resources.jsonl").open("w", encoding="utf-8") as resources:
        # runtime может передать Chromium относительный TMPDIR от этого cwd:
        # socket path остаётся коротким при физическом размещении внутри отчётов.
        child = subprocess.Popen(command, cwd=process_cwd, stdout=stdout, stderr=subprocess.PIPE, env=environment, start_new_session=True)
        observer = ResourceObserver(child, directory.parent)
        selector = selectors.DefaultSelector()
        selector.register(child.stderr, selectors.EVENT_READ)
        next_sample = 0.0
        exited_at = None
        old_handler = signal.getsignal(signal.SIGINT)

        def request_interrupt(_signal=None, _frame=None):
            nonlocal interrupted
            if not interrupted and child.poll() is None:
                interrupted = True
                # Только CLI получает SIGINT и выполняет собственную cleanup.
                child.send_signal(signal.SIGINT)

        signal.signal(signal.SIGINT, request_interrupt)
        try:
            while selector.get_map():
                elapsed = time.monotonic() - start
                if child.poll() is not None:
                    exited_at = elapsed if exited_at is None else exited_at
                    if elapsed - exited_at > 5:
                        # Осиротевший процесс может удерживать pipe. Его ресурсы
                        # сохраняются в отчёте; ожидание EOF не должно быть вечным.
                        break
                if elapsed >= next_sample:
                    append_json(resources, observer.snapshot(elapsed))
                    next_sample = elapsed + SAMPLE_INTERVAL
                for key, _ in selector.select(timeout=0.25):
                    data = os.read(key.fd, 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    stderr.write(data)
                    stderr.flush()
                    if progress:
                        raw_buffer += data
                        while b"\n" in raw_buffer:
                            line, raw_buffer = raw_buffer.split(b"\n", 1)
                            progress.consume(line)
                        if interrupt_after and len(progress.checkpoints) >= interrupt_after:
                            request_interrupt()
            if progress and raw_buffer:
                progress.consume(raw_buffer)
            code = child.wait()
        finally:
            signal.signal(signal.SIGINT, old_handler)
            selector.close()
            child.stderr.close()
            # Ошибка самого runner также должна дать CLI завершить cleanup.
            if child.poll() is None:
                child.send_signal(signal.SIGINT)
                child.wait()
    elapsed = time.monotonic() - start
    try:
        response = json.loads((directory / "stdout.txt").read_text(encoding="utf-8"))
    except (ValueError, UnicodeDecodeError):
        response = None
    result = {
        "returncode": code, "wall_seconds": elapsed, "interrupted": interrupted,
        "response": response, "resources": observer.finish(),
    }
    write_json(directory / "execution.json", result)
    return result


def diagnostic_records(response, run_root):
    if not isinstance(response, dict) or not isinstance(response.get("diagnostic_log"), str):
        return [], unavailable("Итоговый JSON не содержит diagnostic_log")
    path = Path(response["diagnostic_log"]).resolve()
    if not path.is_relative_to(run_root):
        raise AcceptanceError(f"Диагностика записана вне run root: {path}")
    records = []
    try:
        for line in path.read_text(encoding="utf-8").splitlines():
            try:
                value = json.loads(line)
                if isinstance(value, dict) and isinstance(value.get("fields"), dict):
                    records.append(value["fields"])
            except ValueError:
                continue
    except OSError as error:
        return [], unavailable(f"Не удалось прочитать диагностический журнал: {error}")
    return records, {"available": True, "path": str(path), "sha256": digest(path)}


def summarize(progress, execution, run_root, store, command_directory):
    records, diagnostic = diagnostic_records(execution["response"], run_root)
    stages, items = defaultdict(list), []
    stages_by_identity = defaultdict(lambda: defaultdict(list))
    for record in records:
        event = record.get("event")
        if event == "browser_acquisition_stage" and isinstance(record.get("stage_duration_ms"), (int, float)):
            stages[str(record.get("stage", "unknown"))].append(record["stage_duration_ms"])
            stages_by_identity[identity(record)][str(record.get("stage", "unknown"))].append(record["stage_duration_ms"])
        if event == "browser_acquisition_item" and isinstance(record.get("item_duration_ms"), (int, float)):
            items.append(record["item_duration_ms"])
    public_stages = defaultdict(list)
    public_stages_by_identity = defaultdict(lambda: defaultdict(list))
    public_items = []
    for row in progress.checkpoints:
        if isinstance(row["reported_item_elapsed_ms"], (int, float)):
            public_items.append(row["reported_item_elapsed_ms"])
        if not isinstance(row["stage_timings"], list):
            continue
        for timing in row["stage_timings"]:
            if isinstance(timing, dict) and isinstance(timing.get("stage"), str) and isinstance(timing.get("duration_ms"), (int, float)):
                public_stages[timing["stage"]].append(timing["duration_ms"])
                public_stages_by_identity[row["identity"]][timing["stage"]].append(timing["duration_ms"])
    has_diagnostic_stages = bool(stages)
    used_public_fallback = False
    for key, identity_stages in public_stages_by_identity.items():
        if stages_by_identity[key]:
            continue
        used_public_fallback = True
        for stage, durations in identity_stages.items():
            stages[stage].extend(durations)
            stages_by_identity[key][stage].extend(durations)
    stage_source = "diagnostic_log"
    if used_public_fallback:
        stage_source = "diagnostic_log+progress.stage_timings" if has_diagnostic_stages else "progress.stage_timings"
    resource_samples = []
    for line in (command_directory / "resources.jsonl").read_text(encoding="utf-8").splitlines():
        resource_samples.append(json.loads(line))
    enriched_windows = []
    previous_events = Counter()
    previous_elapsed = 0.0
    for index, window in enumerate(progress.rolling):
        enriched = dict(window)
        keys = {row["identity"] for row in progress.checkpoints[index * 100 : (index + 1) * 100]}
        window_stages = defaultdict(list)
        for key in keys:
            for stage, durations in stages_by_identity[key].items():
                window_stages[stage].extend(durations)
        enriched["stage_latency"] = {stage: percentile(durations) for stage, durations in window_stages.items()} or unavailable("Подробные stage timings недоступны")
        current = Counter(window["cumulative_events"])
        enriched["window_events"] = dict(current - previous_events)
        previous_events = current
        end_elapsed = progress.checkpoints[(index + 1) * 100 - 1]["elapsed_ms"]
        if isinstance(end_elapsed, (int, float)):
            window_samples = [sample for sample in resource_samples if previous_elapsed <= sample["elapsed_seconds"] <= end_elapsed / 1000]
            enriched["peak_sampled_browser_rss_bytes"] = max((sample["browser_rss_bytes"] for sample in window_samples), default=None)
            enriched["peak_sampled_chromium_processes"] = max((sample["chromium_processes"] for sample in window_samples), default=None)
            previous_elapsed = end_elapsed / 1000
        enriched_windows.append(enriched)
    write_json(progress.directory / "rolling-windows.json", enriched_windows)
    fault_events = {name: count for name, count in progress.events.items() if name in ("browser_session_fault", "browser_session_failed", "worker_session_failed", "browser_session_recovery_started")}
    failure_codes = Counter(record["failure_code"] for record in records if isinstance(record.get("failure_code"), str))
    resources = execution["resources"]
    report = {
        "wall_seconds": execution["wall_seconds"], "returncode": execution["returncode"],
        "checkpointed": len(progress.checkpoints), "requested": len(progress.requested),
        "checkpoints_per_minute": len(progress.checkpoints) * 60 / execution["wall_seconds"],
        "checkpoint_item_latency": percentile([row["latency_ms"] for row in progress.checkpoints if row["latency_ms"] is not None]),
        "provider_item_latency": percentile(items),
        "reported_item_latency": percentile(public_items),
        "reported_item_latency_missing_count": len(progress.checkpoints) - len(public_items),
        "stage_latency": {name: percentile(values) for name, values in stages.items()} or unavailable("Подробные stage timings недоступны"),
        "stage_latency_source": stage_source,
        "public_stage_timings_missing_count": sum(not isinstance(row["stage_timings"], list) or not row["stage_timings"] for row in progress.checkpoints),
        "outcomes": dict(progress.outcomes), "events": dict(progress.events),
        "diagnostic_failure_codes": dict(failure_codes),
        "diagnostic_failure_count_scope": "События stages/items; одна ошибка может присутствовать в нескольких событиях, это не число уникальных item failures",
        "item_errors": sum(count for outcome, count in progress.outcomes.items() if outcome not in ("candidate_recorded", "unavailable")),
        "session_fault_events": fault_events or unavailable("Нет отдельного подтверждённого event session fault; ноль нельзя вывести из отсутствия поля"),
        "session_end_reasons": dict(progress.session_end_reasons),
        "worker_retirements": progress.events["worker_retired"],
        "worker_exhaustions": progress.events["worker_exhausted"],
        "deferred_assignments": progress.events["item_deferred"],
        "requeued_assignments": progress.events["item_requeued"],
        "retries": progress.events["retry_started"], "rotations": progress.events["browser_session_rotated"],
        "duplicates": progress.duplicates, "unexpected_identities": progress.unexpected,
        "pending_from_progress": sorted(progress.requested - {row["identity"] for row in progress.checkpoints}),
        "non_json_stderr_lines": progress.non_json_lines, "diagnostic_log": diagnostic,
        "resources": resources, "store_disk": disk_usage(store),
        "first_200": window_summary(progress.checkpoints[:200]),
        "last_200": window_summary(progress.checkpoints[-200:]),
        "status": "completed" if len(progress.checkpoints) == len(progress.requested) and not progress.duplicates and not progress.unexpected else "incomplete",
        "acceptance_limit": "Количество terminal outcomes сверяется с batch status; завершённый прогон сам по себе не доказывает отсутствие длительной деградации",
    }
    write_json(progress.directory / "summary.json", report)
    return report


def window_summary(rows):
    times = [row["elapsed_ms"] for row in rows if row["elapsed_ms"] is not None]
    span = (max(times) - min(times)) / 1000 if len(times) > 1 else 0
    return {
        "count": len(rows), "outcomes": dict(Counter(row["outcome"] for row in rows)),
        "item_latency": percentile([row["latency_ms"] for row in rows if row["latency_ms"] is not None]),
        "inter_checkpoint_seconds": span,
        "checkpoints_per_minute": (len(times) - 1) * 60 / span if span > 0 else None,
    }


def verify_durable(response, requested, checkpoint_keys):
    batch = response.get("batch") if isinstance(response, dict) else None
    rows = batch.get("items") if isinstance(batch, dict) else None
    if not isinstance(rows, list):
        return unavailable("batch status не вернул batch.items; durable outcomes не доказаны")
    counts, generations = {}, {}
    for row in rows:
        key = identity(row)
        if not key or key in counts or not isinstance(row.get("attempts"), list):
            raise AcceptanceError("batch status: identity повторяется либо attempts отсутствует")
        counts[key], generations[key] = len(row["attempts"]), row.get("generation")
    errors = {
        "missing": sorted(set(requested) - counts.keys()),
        "unexpected": sorted(counts.keys() - set(requested)),
        "double_attempts": {key: count for key, count in counts.items() if count > 1},
        "checkpoint_not_durable": sorted(key for key in checkpoint_keys if counts.get(key) != 1),
        "durable_without_checkpoint": sorted(key for key, count in counts.items() if count == 1 and key not in checkpoint_keys),
        "unexpected_generation": {key: generation for key, generation in generations.items() if generation != 0},
    }
    return {"available": True, "consistent": not any(errors.values()), "errors": errors, "attempt_counts": counts, "pending": [key for key in requested if counts.get(key) == 0]}


def run_scenario(arguments, root, values, workers, baseline):
    run_id = f"{arguments.mode}-w{workers}-{uuid.uuid4().hex}"
    directory = root / run_id
    directory.mkdir()
    store = directory / "store"
    store.mkdir()
    temporary = directory / "runtime-workspaces"
    temporary.mkdir(mode=0o700)
    environment = dict(os.environ, TMPDIR=str(temporary), ASSET_STORE_TEMP_ROOT=str(temporary))
    corpus = directory / "identities.txt"
    corpus.write_text("".join(value + "\n" for value in values), encoding="utf-8")
    write_json(directory / "scenario.json", {"batch_id": run_id, "workers": workers, "rounds": 1, "count": len(values), "corpus_sha256": digest(corpus), "started_utc": datetime.now(timezone.utc).isoformat()})
    base = [str(arguments.binary), "--store", str(store), "--output", "json", "batch"]
    reports = []
    last_resources = None
    try:
        started = execute_command(base + ["start", "--batch-id", run_id, *values], directory / "start", environment, arguments.repository_root)
        if started["returncode"] != 0:
            raise AcceptanceError(f"batch start завершился кодом {started['returncode']}; см. {directory / 'start'}")
        verify_protection(arguments.repository_root, baseline, directory / "start-protection.json")
        command = base + ["run", "--batch-id", run_id, "--rounds", "1", "--workers", str(workers)]
        if arguments.rotate_after:
            command += ["--acceptance-rotate-after-items", str(arguments.rotate_after)]
        checkpoint_keys = set()
        for phase in ("run", "resume") if arguments.interrupt_after else ("run",):
            progress_dir = directory / (phase + "-metrics")
            progress_dir.mkdir()
            progress = Progress(progress_dir, values)
            try:
                execution = execute_command(command, directory / phase, environment, arguments.repository_root, progress, arguments.interrupt_after if phase == "run" else None)
                last_resources = execution.get("resources")
                report = summarize(progress, execution, directory, store, directory / phase)
                report["phase"] = phase
                reports.append(report)
                if checkpoint_keys & {row["identity"] for row in progress.checkpoints}:
                    raise AcceptanceError("После resume обнаружен повторный checkpoint уже сохранённой identity")
                checkpoint_keys.update(row["identity"] for row in progress.checkpoints)
                report["cumulative_checkpointed"] = len(checkpoint_keys)
                report["cumulative_requested"] = len(values)
                report["cumulative_status"] = "completed" if len(checkpoint_keys) == len(values) else "incomplete"
            finally:
                progress.close()
            verify_protection(arguments.repository_root, baseline, directory / (phase + "-protection.json"))
            status = execute_command(base + ["status", "--batch-id", run_id], directory / (phase + "-status"), environment, arguments.repository_root)
            durable = verify_durable(status["response"], values, checkpoint_keys)
            write_json(directory / (phase + "-durable.json"), durable)
            report["durable"] = durable
            write_json(progress_dir / "summary.json", report)
            response = execution["response"]
            error = response.get("error") if isinstance(response, dict) else None
            permitted_interrupt = phase == "run" and execution["interrupted"] and isinstance(error, dict) and str(error.get("message", "")).startswith("batch_interrupted:")
            if error and not permitted_interrupt:
                raise AcceptanceError(f"CLI сообщил об ошибке: {error}; см. {directory / phase}")
            if not durable.get("available"):
                raise AcceptanceError(f"Не удалось подтвердить durable state; см. {directory / (phase + '-status')}")
            if durable.get("available") and not durable.get("consistent"):
                raise AcceptanceError(f"Попытки и checkpoint не согласованы; см. {directory / (phase + '-durable.json')}")
            resources = execution["resources"]
            orphan_browser = resources["orphan_owned_chromium"]
            if (isinstance(orphan_browser, list) and orphan_browser) or resources["remaining_owned_workspaces"] or resources["owned_workspaces_outside_reports"]:
                raise AcceptanceError(f"Обнаружены оставшиеся либо размещённые вне отчётов ресурсы; см. {directory / phase / 'execution.json'}")
            if phase == "run" and arguments.interrupt_after and not execution["interrupted"]:
                raise AcceptanceError("Smoke завершился до подтверждённого SIGINT; прерывание не проверено")
            if not arguments.interrupt_after or phase == "resume":
                if execution["returncode"] not in (0, 3):
                    raise AcceptanceError(f"CLI завершился ошибкой {execution['returncode']}; см. {directory / phase}")
                if not durable.get("available") or durable.get("pending") or len(checkpoint_keys) != len(values):
                    raise AcceptanceError(f"Прогон неполон; точное состояние сохранено в {directory}")
    except BaseException:
        try:
            write_json(directory / "scenario-results.json", reports)
        except Exception as save_error:
            print(f"Дополнительно: не удалось сохранить результаты сценария: {save_error}", file=sys.stderr)
        try:
            verify_protection(arguments.repository_root, baseline, directory / "repository-protection.json")
        except Exception as protection_error:
            print(f"Дополнительно: {protection_error}", file=sys.stderr)
        raise
    else:
        write_json(directory / "scenario-results.json", reports)
        verify_protection(arguments.repository_root, baseline, directory / "repository-protection.json")
    cleanup_audit = post_run_cleanup_audit(directory, last_resources)
    write_json(directory / "post-run-cleanup.json", cleanup_audit)
    for report in reports:
        report["post_run_cleanup_audit"] = cleanup_audit
        report["resources"]["process_exit_cleanup_proven"] = report["resources"].get("cleanup_proven")
        report["resources"]["cleanup_proven"] = cleanup_audit["cleanup_proven"]
        write_json(directory / f"{report['phase']}-metrics" / "summary.json", report)
    write_json(directory / "scenario-results.json", reports)
    if not cleanup_audit["cleanup_proven"]:
        raise AcceptanceError(f"Post-run cleanup не доказан; см. {directory / 'post-run-cleanup.json'}")
    print(f"Свидетельства сохранены: {directory}", flush=True)
    return {"directory": str(directory), "workers": workers, "reports": reports}


def parse_arguments():
    parser = argparse.ArgumentParser(description="Локальная live-приёмка ограниченного параллелизма Yarxi")
    parser.add_argument("mode", choices=("smoke", "benchmark", "soak"))
    parser.add_argument("--repository-root", type=Path, required=True, help="Корень исходного checkout пользователя")
    parser.add_argument("--binary", type=Path, required=True, help="Уже собранный kanji-assets")
    parser.add_argument("--corpus", type=Path, required=True, help="UTF-8, один уникальный кандзи на строку")
    parser.add_argument("--corpus-sha256", help="Ожидаемый SHA-256 исходного файла корпуса")
    parser.add_argument("--reports-root", type=Path, help=f"По умолчанию <repository-root>/{DEFAULT_REPORTS}")
    parser.add_argument("--workers", type=int, choices=range(1, 5), default=4, help="Workers для smoke/soak; по умолчанию выбранный CLI default 4; benchmark всегда 1/2/3/4")
    parser.add_argument("--smoke-count", type=int, default=60)
    parser.add_argument("--rotate-after", type=int, help="Только smoke: принудительная rotation в acceptance-сборке")
    parser.add_argument("--interrupt-after", type=int, help="Только smoke: SIGINT после N checkpoint и однократный resume")
    arguments = parser.parse_args()
    if not 50 <= arguments.smoke_count <= 100:
        parser.error("--smoke-count должен быть от 50 до 100")
    if arguments.mode != "smoke" and (arguments.rotate_after or arguments.interrupt_after):
        parser.error("Принудительные rotation/SIGINT допускаются только в smoke")
    for name in ("rotate_after", "interrupt_after"):
        value = getattr(arguments, name)
        if value is not None and value <= 0:
            parser.error(f"--{name.replace('_', '-')} должен быть положительным")
    if arguments.interrupt_after and arguments.interrupt_after >= arguments.smoke_count:
        parser.error("--interrupt-after должен быть меньше --smoke-count")
    arguments.repository_root = arguments.repository_root.resolve()
    arguments.binary = arguments.binary.resolve()
    arguments.corpus = arguments.corpus.resolve()
    if not arguments.binary.is_file() or not os.access(arguments.binary, os.X_OK):
        parser.error("--binary должен указывать на существующий исполняемый файл")
    return arguments


def main():
    arguments = parse_arguments()
    expected = {"smoke": arguments.smoke_count, "benchmark": 300, "soak": 1000}[arguments.mode]
    raw, corpus, sha = corpus_input(arguments.corpus, arguments.corpus_sha256, expected)
    repository = arguments.repository_root
    if Path(git(repository, "rev-parse", "--show-toplevel").decode().strip()).resolve() != repository:
        raise AcceptanceError("--repository-root должен быть корнем Git checkout")
    root = ignored_root(repository, arguments.reports_root or repository / DEFAULT_REPORTS)
    lock = root / ".acceptance.lock"
    try:
        lock_file = lock.open("x", encoding="utf-8")
    except FileExistsError as error:
        raise AcceptanceError(f"Приёмка уже заблокирована файлом {lock}; проверьте его владельца перед новым запуском") from error
    with lock_file:
        lock_file.write(json.dumps({"pid": os.getpid(), "created_utc": datetime.now(timezone.utc).isoformat()}) + "\n")
        lock_file.flush()
        session = root / ("session-" + uuid.uuid4().hex)
        session.mkdir()
        try:
            (session / "corpus.txt").write_bytes(raw)
            write_json(session / "inputs.json", {
                "mode": arguments.mode, "corpus_sha256": sha, "corpus_count": len(corpus),
                "selected_count": expected, "selection": "Первые N identities исходного упорядоченного корпуса",
                "binary": str(arguments.binary), "binary_sha256": digest(arguments.binary),
                "repository_root": str(repository), "revision": git(repository, "rev-parse", "HEAD").decode().strip(),
            })
            baseline = protected_state(repository)
            write_json(session / "protected-before.json", baseline)
            results = []
            workers = (1, 2, 3, 4) if arguments.mode == "benchmark" else (arguments.workers,)
            for count in workers:
                results.append(run_scenario(arguments, root, corpus[:expected], count, baseline))
                if digest(arguments.corpus) != sha:
                    raise AcceptanceError("Исходный корпус изменился во время приёмки")
            write_json(session / "results.json", results)
            print(f"Приёмка завершена; сводный индекс: {session / 'results.json'}")
        except Exception as error:
            write_json(session / "failure.json", {"error": str(error), "type": type(error).__name__, "artifacts_retained": True})
            raise
        finally:
            # Удаляется только блокировка этого runner, свидетельства сохраняются.
            lock.unlink()


if __name__ == "__main__":
    try:
        main()
    except (AcceptanceError, OSError) as error:
        print(f"Приёмка остановлена: {error}", file=sys.stderr)
        raise SystemExit(1)
