"""Synthetic fixtures for the user-data tracking guard."""

import unittest
from pathlib import Path

from check_local_data_untracked import (
    ignore_rule_problems,
    tracked_user_data,
)

GITIGNORE = """
# Пользовательские данные никогда не публикуются.
/decks/
/.asset-store/
"""


class TrackedUserDataTests(unittest.TestCase):
    def test_user_areas_are_reported(self):
        tracked = [
            "decks/input/Hidden/deck.json",
            "decks/output/Words__N5/deck.json",
            "decks/output/Words__N5/media/word.mp3",
            ".asset-store/kanji/manifest.json",
            ".asset-store/kanji/assets/png/x.png",
            "decks/deck.json",
            # Любой путь внутри `decks/` — пользовательские данные, а не только
            # две названные подобласти: прежняя раскладка с каталогами
            # группировки тоже не публикуется.
            "decks/legacy/words/Words__N5/deck.json",
            "decks/README.md",
        ]
        self.assertEqual(tracked_user_data(tracked), tracked)

    def test_tooling_paths_are_not_reported(self):
        tracked = [
            "README.md",
            "tools/anki-repo/src/layout.rs",
            "docs/asset-store.md",
            "tools/asset-store/src/store.rs",
        ]
        self.assertEqual(tracked_user_data(tracked), [])

    def test_leading_dot_slash_is_normalized(self):
        self.assertEqual(
            tracked_user_data(["./decks/output/Words__A/deck.json"]),
            ["decks/output/Words__A/deck.json"],
        )

    def test_backslash_separator_is_normalized(self):
        self.assertEqual(
            tracked_user_data(["decks\\input\\Hidden\\deck.json"]),
            ["decks/input/Hidden/deck.json"],
        )


class IgnoreRuleTests(unittest.TestCase):
    def test_complete_rules_pass(self):
        self.assertEqual(ignore_rule_problems(GITIGNORE), [])

    def test_missing_rule_is_reported(self):
        text = "/decks/input/\n/.asset-store/\n"
        problems = ignore_rule_problems(text)
        self.assertTrue(any("/decks/" in problem for problem in problems))

    def test_narrow_rule_does_not_cover_the_whole_area(self):
        text = "/decks/input/\n/decks/output/\n/.asset-store/\n"
        problems = ignore_rule_problems(text)
        self.assertTrue(any("/decks/" in problem for problem in problems))

    def test_re_inclusion_is_reported(self):
        text = GITIGNORE + "!/.asset-store/kanji/manifest.json\n"
        problems = ignore_rule_problems(text)
        self.assertTrue(any("повторно включает" in problem for problem in problems))

    def test_unrelated_negation_is_allowed(self):
        text = GITIGNORE + "!*.example\n"
        self.assertEqual(ignore_rule_problems(text), [])

    def test_comment_mentioning_an_area_is_not_a_rule(self):
        text = GITIGNORE + "# !/.asset-store/\n"
        self.assertEqual(ignore_rule_problems(text), [])


class RepositoryStateTests(unittest.TestCase):
    def test_this_repository_passes_its_own_guard(self):
        root = Path(__file__).resolve().parents[2]
        from check_local_data_untracked import validate_tree

        self.assertEqual(validate_tree(root), [])


if __name__ == "__main__":
    unittest.main()
