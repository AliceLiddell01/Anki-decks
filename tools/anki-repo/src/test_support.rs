//! Минимальные in-memory helpers для unit tests.
//!
//! Компилируется только в test-режиме и не входит в публичный контракт.

use std::path::Path;

use crate::model::DeckNode;
use asset_store::temp_workspace::TempWorkspace;

/// Временный каталог для unit tests, удаляемый вместе со значением.
pub struct TempDir {
    workspace: TempWorkspace,
}

impl TempDir {
    /// Создаёт пустой временный каталог.
    ///
    /// # Panics
    ///
    /// Паникует, если каталог невозможно создать: это ошибка окружения.
    #[must_use]
    pub fn new(label: &str) -> Self {
        let workspace = TempWorkspace::create(label).expect("временный каталог должен создаваться");
        Self { workspace }
    }

    /// Путь каталога.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.workspace.path()
    }
}

/// Разбирает CrowdAnki JSON из строки.
///
/// # Panics
///
/// Паникует, если тестовый JSON некорректен: это ошибка теста, а не домена.
#[must_use]
pub fn deck_node(json: &str) -> DeckNode {
    serde_json::from_str(json).expect("тестовый CrowdAnki JSON должен разбираться")
}

/// Минимальный валидный экспорт: одна колода, одна модель, одна конфигурация.
pub const MINIMAL_EXPORT: &str = r#"{
  "__type__": "Deck",
  "name": "Test::Deck",
  "crowdanki_uuid": "deck-uuid-1",
  "deck_config_uuid": "cfg-1",
  "children": [],
  "media_files": ["a.mp3", "b.png"],
  "note_models": [
    {
      "__type__": "NoteModel",
      "crowdanki_uuid": "model-1",
      "name": "Тестовая модель",
      "css": "",
      "flds": [
        {"name": "Заголовок", "ord": 0},
        {"name": "Толкование", "ord": 1}
      ],
      "tmpls": [
        {"name": "Карточка 1", "ord": 0, "qfmt": "{{Заголовок}}", "afmt": "{{FrontSide}}{{Толкование}}"}
      ],
      "x_unknown_model_key": {"keep": true}
    }
  ],
  "deck_configurations": [
    {"__type__": "DeckConfig", "crowdanki_uuid": "cfg-1", "name": "По умолчанию"}
  ],
  "notes": [
    {
      "__type__": "Note",
      "guid": "guid-1",
      "note_model_uuid": "model-1",
      "tags": ["тэг"],
      "fields": ["[sound:a.mp3]偶然", "случайность"],
      "x_unknown_note_key": 7
    },
    {
      "__type__": "Note",
      "guid": "guid-2",
      "note_model_uuid": "model-1",
      "fields": ["必然", ""],
      "tags": []
    }
  ],
  "x_unknown_root_key": {"keep": true}
}"#;

/// Экспорт с вложенной дочерней колодой и разными путями колод.
pub const NESTED_EXPORT: &str = r#"{
  "__type__": "Deck",
  "name": "Root",
  "crowdanki_uuid": "deck-root",
  "deck_config_uuid": "cfg-1",
  "children": [
    {
      "__type__": "Deck",
      "name": "Root::Child",
      "crowdanki_uuid": "deck-child",
      "deck_config_uuid": "cfg-1",
      "children": [
        {
          "__type__": "Deck",
          "name": "Root::Child::Leaf",
          "crowdanki_uuid": "deck-leaf",
          "deck_config_uuid": "cfg-1",
          "children": [],
          "notes": [
            {"guid": "guid-leaf", "note_model_uuid": "model-1", "fields": ["深い", "глубокий"], "tags": []}
          ],
          "media_files": [],
          "note_models": [],
          "deck_configurations": []
        }
      ],
      "notes": [
        {"guid": "guid-child", "note_model_uuid": "model-1", "fields": ["子", "ребёнок"], "tags": []}
      ],
      "media_files": [],
      "note_models": [],
      "deck_configurations": []
    }
  ],
  "notes": [
    {"guid": "guid-root", "note_model_uuid": "model-1", "fields": ["根", "корень"], "tags": []}
  ],
  "media_files": [],
  "note_models": [
    {
      "__type__": "NoteModel",
      "crowdanki_uuid": "model-1",
      "name": "Тестовая модель",
      "css": "",
      "flds": [
        {"name": "Заголовок", "ord": 0},
        {"name": "Толкование", "ord": 1}
      ],
      "tmpls": [
        {"name": "Карточка 1", "ord": 0, "qfmt": "{{Заголовок}}", "afmt": "{{Толкование}}"}
      ]
    }
  ],
  "deck_configurations": [
    {"__type__": "DeckConfig", "crowdanki_uuid": "cfg-1", "name": "По умолчанию"}
  ]
}"#;

/// Строит текст `deck.json` из базового экспорта с произвольной мутацией.
#[must_use]
pub fn export_with(base: &str, mutate: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut value: serde_json::Value =
        serde_json::from_str(base).expect("базовый тестовый JSON должен разбираться");
    mutate(&mut value);
    serde_json::to_string(&value).expect("тестовый JSON должен сериализоваться")
}

#[cfg(test)]
mod tests {
    use super::TempDir;
    use std::path::PathBuf;

    fn fixture_that_fails_early(path_out: &mut Option<PathBuf>) -> Result<(), &'static str> {
        let dir = TempDir::new("anki-repo-unit-early-error");
        *path_out = Some(dir.path().to_path_buf());
        Err("synthetic setup failure")
    }

    #[test]
    fn dropping_fixture_removes_its_workspace() {
        let path = {
            let dir = TempDir::new("anki-repo-unit-drop");
            let path = dir.path().to_path_buf();
            assert!(path.is_dir());
            path
        };

        assert!(!path.exists());
    }

    #[test]
    fn early_error_drops_fixture_workspace() {
        let mut path = None;
        let error = fixture_that_fails_early(&mut path).expect_err("fixture завершается ошибкой");

        assert_eq!(error, "synthetic setup failure");
        assert!(!path.expect("путь fixture").exists());
    }
}
