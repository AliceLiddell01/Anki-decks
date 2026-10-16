#!/usr/bin/env python3
"""Guard: user data must never be tracked by Git (stdlib only).

Пользовательские данные репозитория — локальные колоды и локальное доверенное
хранилище ресурсов — живут в игнорируемых Git областях:

* ``decks/``         — пользовательские данные целиком: ``decks/input/`` —
  закрытая область будущего доверенного загрузчика, ``decks/output/`` —
  плоский список рабочих локальных колод. Отслеживаемым внутри ``decks/``
  не может быть ни один путь, а не только эти две подобласти;
* ``.asset-store/``  — локальное доверенное хранилище ресурсов.

``.gitignore`` сам по себе не защищает: он не отменяет уже добавленный индекс и
обходится ``git add -f``. Поэтому проверка идёт по фактическому индексу, а
дополнительно подтверждается, что правила игнорирования на месте и не ослаблены
повторным включением.
"""

import argparse
import subprocess
import sys
from pathlib import Path

# Области, которые не могут быть отслеживаемыми ни при каких условиях.
FORBIDDEN_PREFIXES = (
    "decks/",
    ".asset-store/",
)

# Правила, которые обязаны присутствовать в корневом `.gitignore`.
REQUIRED_IGNORE_RULES = ("/decks/", "/.asset-store/")

# Маркер повторного включения в `.gitignore`.
NEGATION = "!"


def tracked_user_data(tracked_paths):
    """Возвращает отслеживаемые пути, лежащие в пользовательских областях."""
    violations = []
    for path in tracked_paths:
        normalized = path.replace("\\", "/")
        # Отбрасывается только буквальный префикс `./`: `.asset-store/` начинается
        # с точки, и вырезание множества символов уничтожило бы его имя.
        if normalized.startswith("./"):
            normalized = normalized[2:]
        if normalized.startswith(FORBIDDEN_PREFIXES):
            violations.append(normalized)
    return violations


def ignore_rule_problems(gitignore_text):
    """Возвращает проблемы правил игнорирования пользовательских областей."""
    problems = []
    lines = [line.strip() for line in gitignore_text.splitlines()]
    rules = {line for line in lines if line and not line.startswith("#")}

    for rule in REQUIRED_IGNORE_RULES:
        if rule not in rules:
            problems.append(f"в корневом .gitignore отсутствует правило {rule}")

    for line in lines:
        if not line or line.startswith("#") or not line.startswith(NEGATION):
            continue
        re_included = line[len(NEGATION):].strip().lstrip("/")
        for prefix in FORBIDDEN_PREFIXES:
            bare = prefix.rstrip("/")
            if re_included == bare or re_included.startswith(prefix):
                problems.append(
                    f"правило .gitignore повторно включает пользовательскую область: {line}"
                )
                break

    return problems


def tracked_paths(root):
    """Читает индекс Git; отсутствие git — ошибка проверки, а не пропуск."""
    result = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=root,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(f"не удалось прочитать индекс Git: {detail or result.returncode}")
    return [
        item.decode("utf-8", "surrogateescape")
        for item in result.stdout.split(b"\0")
        if item
    ]


def validate_tree(root):
    """Возвращает список нарушений границы пользовательских данных."""
    gitignore = root / ".gitignore"
    if not gitignore.is_file():
        return ["в корне репозитория нет .gitignore"]
    problems = ignore_rule_problems(gitignore.read_text(encoding="utf-8"))
    for path in tracked_user_data(tracked_paths(root)):
        problems.append(f"пользовательский путь отслеживается Git: {path}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    try:
        errors = validate_tree(args.root)
    except RuntimeError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("Пользовательские данные не отслеживаются Git, правила игнорирования на месте")
    return 0


if __name__ == "__main__":
    sys.exit(main())
