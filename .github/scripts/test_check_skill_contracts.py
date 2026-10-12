"""Synthetic fixtures for the Agent Skills metadata checker."""

import tempfile
import unittest
from pathlib import Path

from check_skill_contracts import validate_skill, validate_tree


class SkillContractTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.folder = self.root / ".agents/skills/sample-skill"
        self.folder.mkdir(parents=True)
        self.file = self.folder / "SKILL.md"

    def write_skill(self, metadata=None, body="See [details](references/details.md)."):
        self.file.write_text(metadata or "---\nname: sample-skill\ndescription: >-\n  Use this to perform a concrete skill task.\n---\n" + body, encoding="utf-8")

    def test_valid_skill_and_local_ref(self):
        (self.folder / "references").mkdir()
        (self.folder / "references/details.md").write_text("Details", encoding="utf-8")
        self.write_skill()
        self.assertEqual(validate_tree(self.root), [])

    def test_invalid_name_and_nonstandard_routing_key(self):
        self.write_skill("---\nname: wrong-name\ndescription: Demo skill.\nwhenToUse: never\n---\n")
        self.assertTrue(any("name" in x for x in validate_skill(self.file)))
        self.assertTrue(any("whenToUse" in x for x in validate_skill(self.file)))

    def test_missing_reference(self):
        self.write_skill()
        self.assertTrue(any("ссылка" in x for x in validate_skill(self.file)))

    def test_description_limit(self):
        self.write_skill("---\nname: sample-skill\ndescription: >-\n  " + "x" * 1025 + "\n---\n")
        self.assertTrue(any("1024" in x for x in validate_skill(self.file)))

    def test_missing_frontmatter(self):
        self.write_skill("# Just text")
        self.assertTrue(any("frontmatter" in x for x in validate_skill(self.file)))

    def test_directory_without_manifest_fails(self):
        self.write_skill()
        (self.folder / "SKILL.md").unlink()
        self.assertTrue(any("SKILL.md" in x for x in validate_tree(self.root)))

    def test_broken_yaml_values_are_rejected(self):
        for value in ("[broken", '"unterminated', "'unterminated", "{a: 1", "trailing,", '"первая" "вторая"'):
            with self.subTest(value=value):
                self.write_skill(f"---\nname: sample-skill\ndescription: {value}\n---\n")
                self.assertTrue(any("неразбираемое" in x for x in validate_skill(self.file)))

    def test_empty_block_scalars_are_rejected(self):
        for marker in ("|", "|-", "|+", ">", ">-", ">+"):
            with self.subTest(marker=marker):
                self.write_skill(f"---\nname: sample-skill\ndescription: {marker}\n---\n")
                self.assertTrue(any("1024" in x for x in validate_skill(self.file)), marker)

    def test_filled_block_scalar_is_accepted(self):
        for marker in ("|", "|+", ">-"):
            with self.subTest(marker=marker):
                self.write_skill(f"---\nname: sample-skill\ndescription: {marker}\n  Достаточное описание.\n---\n", body="Без ссылок.")
                self.assertEqual(validate_skill(self.file), [])

    def test_flow_collections_are_accepted(self):
        # `metadata` — YAML-отображение, поэтому корректные flow-структуры допустимы.
        for value in ("{author: x}", "{author: [a, b]}", "{author: '}'}"):
            with self.subTest(value=value):
                self.write_skill(f"---\nname: sample-skill\ndescription: Demo.\nmetadata: {value}\n---\n", body="Без ссылок.")
                self.assertEqual(validate_skill(self.file), [])

    def test_unbalanced_flow_collections_are_rejected(self):
        for value in ("{author: x", "[broken", "{author: x} extra", "}"):
            with self.subTest(value=value):
                self.write_skill(f"---\nname: sample-skill\ndescription: Demo.\nmetadata: {value}\n---\n", body="Без ссылок.")
                self.assertTrue(any("flow" in x for x in validate_skill(self.file)), value)

    def test_valid_quoted_and_block_scalars(self):
        self.write_skill('---\nname: sample-skill\ndescription: "Краткое описание."\n---\n', body="Без ссылок.")
        self.assertEqual(validate_skill(self.file), [])
        self.write_skill("---\nname: sample-skill\ndescription: 'It''s fine.'\n---\n", body="Без ссылок.")
        self.assertEqual(validate_skill(self.file), [])
        self.write_skill(body="Без ссылок.")
        self.assertEqual(validate_skill(self.file), [])


if __name__ == "__main__":
    unittest.main()
