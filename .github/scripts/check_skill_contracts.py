#!/usr/bin/env python3
"""Check repository Agent Skills metadata and local reference links (stdlib only)."""

import argparse
import re
import sys
from pathlib import Path

NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\Z")
KEY = re.compile(r"^([A-Za-z][A-Za-z0-9_-]*):(?:\s*(.*))?$")
LINK = re.compile(r"\]\(([^)]+)\)")
ALLOWED = {"name", "description", "license", "compatibility", "metadata", "allowed-tools"}
# Все формы блочных скаляров YAML: значение продолжается отступленными строками.
BLOCK_SCALARS = (">", ">-", ">+", "|", "|-", "|+")
# Flow-структуры YAML допустимы (`metadata: {author: x}`), недопустимы только
# незакрытые или несбалансированные: они делают frontmatter неразбираемым.
FLOW_MAP = {"[": "]", "{": "}"}
CLOSING = set(FLOW_MAP.values())


def flow_error(value: str) -> str | None:
    """Check a value that starts with a flow bracket: balance and trailing text."""
    stack: list[str] = []
    quote = ""
    index = 0
    while index < len(value):
        char = value[index]
        if quote:
            if quote == '"' and char == "\\":
                index += 2
                continue
            if char == quote:
                # Удвоенная одинарная кавычка — escape внутри одинарного скаляра.
                if quote == "'" and value[index + 1:index + 2] == "'":
                    index += 2
                    continue
                quote = ""
            index += 1
            continue
        if char in ("'", '"'):
            quote = char
        elif char in FLOW_MAP:
            stack.append(FLOW_MAP[char])
        elif char in CLOSING:
            if not stack or stack.pop() != char:
                return "несбалансированная flow-скобка"
            if not stack and value[index + 1:].strip():
                return "текст после flow-структуры"
        index += 1
    if quote:
        return "незакрытая кавычка"
    if stack:
        return "незакрытая flow-скобка"
    return None


def quoted_error(value: str) -> str | None:
    """Check a value that starts with a quote; return a reason if it is not one scalar."""
    quote = value[0]
    index = 1
    while index < len(value):
        char = value[index]
        if quote == '"' and char == "\\":
            index += 2
            continue
        if char == quote:
            # В одинарных кавычках удвоенная кавычка — это escape, а не конец значения.
            if quote == "'" and index + 1 < len(value) and value[index + 1] == "'":
                index += 2
                continue
            return None if not value[index + 1:].strip() else "текст после закрывающей кавычки"
        index += 1
    return "незакрытая кавычка"


def syntax_error(value: str) -> str | None:
    """Return a reason when a frontmatter value is not a parsable YAML scalar."""
    if value.startswith(("'", '"')):
        return quoted_error(value)
    if value.startswith(("[", "{")):
        return flow_error(value)
    if value.endswith(("[", "{", "]", "}")):
        return "незакрытая flow-скобка"
    if value.endswith(","):
        return "непарная flow-скобка"
    if ": " in value or value.endswith(":"):
        return "неожиданное ':' внутри скаляра"
    if value.endswith("\\"):
        return "висячий escape"
    return None


def validate_skill(path: Path) -> list[str]:
    errors: list[str] = []
    try:
        source = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        return [f"{path}: не удалось прочитать: {error}"]
    lines = source.splitlines()
    if not lines or lines[0] != "---":
        return [f"{path}: отсутствует начальный YAML frontmatter"]
    try:
        end = lines.index("---", 1)
    except ValueError:
        return [f"{path}: отсутствует закрывающий ---"]
    header = lines[1:end]
    entries: dict[str, tuple[int, str]] = {}
    for i, line in enumerate(header):
        if not line or line.startswith((" ", "#")):
            continue
        match = KEY.fullmatch(line)
        if not match:
            errors.append(f"{path}: неверная запись frontmatter, строка {i + 2}")
            continue
        key, value = match.group(1), (match.group(2) or "").strip()
        if key in entries:
            errors.append(f"{path}: повторяющийся YAML-ключ {key}")
        else:
            entries[key] = (i, value)
        if key not in ALLOWED:
            errors.append(f"{path}: нестандартное поле {key}; условия активации помещай в description")
        # Блочный скаляр (`|`, `>`) продолжается отступленными строками и
        # проверяется отдельно при разборе description.
        if value not in BLOCK_SCALARS:
            reason = syntax_error(value)
            if reason:
                errors.append(f"{path}: неразбираемое значение {key}: {reason}")
    name = entries.get("name", (-1, ""))[1]
    if not NAME.fullmatch(name) or len(name) > 64 or name != path.parent.name:
        errors.append(f"{path}: name должен совпадать с каталогом и отвечать Agent Skills spec")
    if "description" not in entries:
        errors.append(f"{path}: отсутствует description")
    else:
        start, value = entries["description"]
        if value in BLOCK_SCALARS:
            next_key = next((i for i in range(start + 1, len(header)) if KEY.fullmatch(header[i])), len(header))
            text = " ".join(line.strip() for line in header[start + 1:next_key] if line.startswith((" ", "\t"))).strip()
        elif value.startswith(("'", '"')):
            text = value[1:-1]
        else:
            text = value
        if not text or len(text) > 1024:
            errors.append(f"{path}: description должен содержать 1–1024 символа")
    body = "\n".join(lines[end + 1:])
    for match in LINK.finditer(body):
        target = match.group(1).split("#", 1)[0]
        if not target or ":" in target or target.startswith(("/", "#")):
            continue
        resolved = (path.parent / target).resolve()
        if not resolved.is_relative_to(path.parent.resolve()) or not resolved.is_file():
            errors.append(f"{path}: несуществующая или выходящая за skill ссылка: {target}")
    return errors


def validate_tree(root: Path) -> list[str]:
    base = root / ".agents" / "skills"
    directories = sorted((path for path in base.glob("*") if path.is_dir()), key=lambda path: path.name)
    if not directories:
        return ["Нет каталогов skills в .agents/skills/"]
    # Каталог skill без манифеста — тоже нарушение контракта: обход по
    # `*/SKILL.md` такую поломку не заметил бы.
    missing = [f"{directory}: отсутствует SKILL.md" for directory in directories if not (directory / "SKILL.md").is_file()]
    return missing + [error for skill in sorted(base.glob("*/SKILL.md")) for error in validate_skill(skill)]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    errors = validate_tree(args.root)
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("Метаданные и ссылки Agent Skills соответствуют контракту")
    return 0


if __name__ == "__main__":
    sys.exit(main())
