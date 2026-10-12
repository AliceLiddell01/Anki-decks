"""Negative and positive contract tests for PR body formatting."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from check_pr_body import is_automation_pr, validate

HEADINGS = (
    "Цель",
    "Область изменений",
    "Реализация",
    "Проверки",
    "CI",
    "Готовность",
    "Ограничения",
)


def valid_body() -> str:
    return "\n\n".join(f"## {name}\nФактическая информация для раздела {name}." for name in HEADINGS)


class PrBodyContractTests(unittest.TestCase):
    def test_valid_body(self):
        self.assertEqual(validate(valid_body()), [])

    def test_absent_headings(self):
        self.assertEqual(len(validate("## Что изменено\nСделано.")), len(HEADINGS))

    def test_headings_in_code_fence_are_not_real(self):
        self.assertEqual(len(validate("```md\n" + valid_body() + "\n```")), len(HEADINGS))

    def test_inner_info_string_does_not_close_fence(self):
        # Строка с info string внутри открытого блока — содержимое, а не его конец,
        # поэтому заголовки примера не становятся разделами реального body.
        nested = "```text\n```md\n" + valid_body() + "\n```\n"
        self.assertEqual(len(validate(nested)), len(HEADINGS))

    def test_closing_fence_requires_bare_marker(self):
        body = "```\n" + valid_body() + "\n```python\n```\n"
        self.assertEqual(len(validate(body)), len(HEADINGS))

    def test_tilde_and_longer_backtick_fences(self):
        self.assertEqual(len(validate("~~~md\n" + valid_body() + "\n~~~")), len(HEADINGS))
        # Более длинный маркер открывает блок, который не закрывается более коротким,
        # поэтому заголовки внутри остаются примером, а не разделами.
        self.assertEqual(len(validate("````md\n```\n" + valid_body() + "\n````")), len(HEADINGS))
        # Более длинный маркер способен закрыть блок, открытый коротким, но всё,
        # что уже лежало внутри, остаётся примером, а не разделом body.
        self.assertEqual(len(validate("```md\n" + valid_body() + "\n````")), len(HEADINGS))

    def test_headings_in_html_comment_are_not_real(self):
        # GitHub скрывает HTML-комментарии, поэтому шаблон внутри них не является телом PR.
        self.assertEqual(len(validate(f"<!--\n{valid_body()}\n-->")), len(HEADINGS))

    def test_unterminated_comment_hides_rest(self):
        body = f"## Цель\nИнформация.\n<!-- {chr(10)}" + "\n\n".join(f"## {n}\nИнформация." for n in HEADINGS[1:])
        self.assertTrue(any("Область изменений" in e for e in validate(body)))

    def test_inline_comment_keeps_section_text(self):
        body = valid_body().replace("Фактическая информация для раздела CI.", "<!-- пояснение -->\nФакт CI.")
        self.assertEqual(validate(body), [])

    def test_comment_marker_inside_fence_is_literal(self):
        # Внутри код-блока `<!--` — пример, а не комментарий, поэтому описанный
        # в примере заголовок не становится разделом реального body.
        self.assertEqual(len(validate("```md\n<!--\n" + valid_body() + "\n-->\n```")), len(HEADINGS))
        documented = valid_body() + "\n\n```md\n<!-- скрытый пример -->\n```"
        self.assertEqual(validate(documented), [])

    def test_atx_closing_hashes_are_not_part_of_heading_name(self):
        # `## Цель ##` — корректный ATX-заголовок, и GitHub показывает имя «Цель».
        self.assertEqual(validate("\n\n".join(f"## {n} ##\nИнформация." for n in HEADINGS)), [])
        self.assertEqual(validate("\n\n".join(f"## {n} ###\nИнформация." for n in HEADINGS)), [])

    def test_fenced_content_does_not_make_section_nonempty(self):
        body = valid_body().replace("Фактическая информация для раздела CI.", "```md\nThis is a sample, not CI evidence.\n```")
        self.assertTrue(any("Пустой" in e for e in validate(body)))

    def test_coderabbit_table_in_fence_is_not_accepted(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\n```markdown\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| 1 · x | — | — | 0 | clean | x |\n```"
        self.assertTrue(any("таблицы" in e for e in validate(body)))

    def test_duplicate_and_reordered_headings(self):
        body = valid_body() + "\n\n## Цель\nПовторный раздел."
        self.assertTrue(any("повторяется" in e for e in validate(body)))
        a, b = "## Цель\n", "## Реализация\n"
        self.assertTrue(any("порядке" in e for e in validate(valid_body().replace(a, "TEMP\n").replace(b, a).replace("TEMP\n", b))))

    def test_empty_and_placeholder_sections(self):
        self.assertTrue(any("Пустой" in e for e in validate(valid_body().replace("Фактическая информация для раздела CI.", ""))))
        self.assertTrue(any("заглушку" in e for e in validate(valid_body().replace("Фактическая информация для раздела CI.", "TODO"))))

    def test_conditional_coderabbit_table(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\nПроверено."
        self.assertTrue(any("таблицы" in e for e in validate(body)))
        body += "\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 findings | clean | `def` |"
        self.assertEqual(validate(body), [])

    def test_coderabbit_row_requires_reviewed_sha(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 | — | — | 0 findings | clean | unknown |"
        self.assertTrue(any("строка" in e for e in validate(body)))

    def test_coderabbit_row_requires_filled_cells(self):
        # Строка из одних разделителей не является записью disposition.
        table = "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n"
        self.assertTrue(any("строка" in e for e in validate(valid_body() + table + "| 1 · `abcdef0` | | | | | |")))
        self.assertEqual(validate(valid_body() + table + "| 1 · `abcdef0` | — | — | 0 findings | clean | `abcdef0` |"), [])

    def test_bot_exception_is_limited_to_account_and_branch(self):
        def payload(login, branch):
            return {"pull_request": {"user": {"login": login}, "head": {"ref": branch}}}
        self.assertTrue(is_automation_pr(payload("renovate[bot]", "renovate/rust-sha2")))
        self.assertTrue(is_automation_pr(payload("dependabot[bot]", "dependabot/github_actions/action")))
        self.assertFalse(is_automation_pr(payload("someone", "renovate/fake")))
        self.assertFalse(is_automation_pr(payload("renovate[bot]", "feat/other")))

    def test_cli_rejects_event_without_pull_request(self):
        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "edited"}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 2)
            self.assertIn("pull_request", proc.stderr)


if __name__ == "__main__":
    unittest.main()
