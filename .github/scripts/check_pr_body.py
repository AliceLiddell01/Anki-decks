#!/usr/bin/env python3
"""Validate the repository's human-authored PR body contract (stdlib only)."""

import argparse
import json
import re
import sys
from pathlib import Path

REQUIRED = (
    "Цель",
    "Область изменений",
    "Реализация",
    "Проверки",
    "CI",
    "Готовность",
    "Ограничения",
)
BOT_BRANCHES = {"renovate[bot]": "renovate/", "dependabot[bot]": "dependabot/"}
HEADING = re.compile(r"^ {0,3}##\s+(.+?)\s*$")
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")


def sections(body: str) -> list[tuple[str, str]]:
    """Extract level-two headings and their text, ignoring fenced code examples."""
    found: list[tuple[str, str]] = []
    active: list[str] = []
    fence_char = ""
    fence_len = 0
    for line in body.replace("\r\n", "\n").replace("\r", "\n").split("\n"):
        marker = FENCE.match(line)
        if marker:
            token = marker.group(1)
            if not fence_char:
                fence_char, fence_len = token[0], len(token)
            elif token[0] == fence_char and len(token) >= fence_len:
                fence_char = ""
            if found and not fence_char:
                active.append(line)
            continue
        if fence_char:
            if found:
                active.append(line)
            continue
        match = HEADING.match(line)
        if match:
            if found:
                found[-1] = (found[-1][0], "\n".join(active))
            found.append((match.group(1).strip(), ""))
            active = []
        elif found:
            active.append(line)
    if found:
        found[-1] = (found[-1][0], "\n".join(active))
    return found


def validate(body: str) -> list[str]:
    """Return deterministic errors; empty list means structurally valid."""
    blocks = sections(body)
    names = [heading for heading, _ in blocks]
    errors: list[str] = []
    for name in REQUIRED:
        count = names.count(name)
        if count == 0:
            errors.append(f"Отсутствует обязательный заголовок: ## {name}")
        elif count > 1:
            errors.append(f"Заголовок повторяется: ## {name}")
    if all(name in names for name in REQUIRED):
        positions = [names.index(name) for name in REQUIRED]
        if positions != sorted(positions):
            errors.append("Обязательные разделы расположены не в установленном порядке")
    for name, content in blocks:
        if name in REQUIRED and not content.strip():
            errors.append(f"Пустой обязательный раздел: ## {name}")
        if name in REQUIRED and content.strip().casefold() in {"todo", "tbd", "заполнить", "уточнить"}:
            errors.append(f"Раздел содержит только заглушку: ## {name}")
    if "CodeRabbit review и disposition" in names:
        if names.count("CodeRabbit review и disposition") > 1:
            errors.append("Раздел CodeRabbit повторяется")
        for name, content in blocks:
            if name == "CodeRabbit review и disposition":
                if "| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |" not in content:
                    errors.append("В разделе CodeRabbit отсутствует заголовок накопительной таблицы")
                if not re.search(r"(?m)^\|\s*\d+\s*[·|]", content):
                    errors.append("В разделе CodeRabbit отсутствует строка завершённой итерации")
    return errors


def is_automation_pr(payload: dict) -> bool:
    """Narrow exception for dependency PRs authored by verified bot identities."""
    pr = payload.get("pull_request") or {}
    login = (pr.get("user") or {}).get("login", "")
    branch = (pr.get("head") or {}).get("ref", "")
    return branch.startswith(BOT_BRANCHES.get(login, "\0"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--body-file", type=Path, help="Local Markdown source before PR publication")
    source.add_argument("--event-path", type=Path, help="GitHub pull_request event JSON")
    args = parser.parse_args()
    try:
        if args.body_file is not None:
            body = args.body_file.read_text(encoding="utf-8")
        else:
            payload = json.loads(args.event_path.read_text(encoding="utf-8"))
            if not isinstance(payload, dict) or not isinstance(payload.get("pull_request"), dict):
                raise ValueError("Нет объекта pull_request в событии")
            if is_automation_pr(payload):
                print("PR dependency-бота: формат ручных PR не применяется")
                return 0
            body = payload["pull_request"].get("body") or ""
        problems = validate(body)
    except (OSError, UnicodeError, ValueError) as exc:
        print(f"Не удалось проверить PR body: {exc}", file=sys.stderr)
        return 2
    if problems:
        for problem in problems:
            print(f"::error::{problem}", file=sys.stderr)
        return 1
    print("Структура PR body соответствует контракту")
    return 0


if __name__ == "__main__":
    sys.exit(main())
