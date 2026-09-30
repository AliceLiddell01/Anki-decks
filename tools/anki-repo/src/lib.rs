//! `anki-repo` — toolkit для CrowdAnki-экспортов репозитория Anki-decks.
//!
//! Библиотечная часть отделена от executable boundary: тонкий [`crate::cli`]
//! и [`crate::run`] снаружи, но загрузка CrowdAnki, обход дерева, индексы,
//! domain-операции, validation и output contracts тестируются напрямую.
//!
//! `inspect`, `find`, `stats`, `validate`, `qa`, `review`, `models` и
//! `visual-report` только читают. `review-check` тоже ничего не пишет: он
//! проверяет proposals внешнего reviewer'а и компилирует их в запрос для `edit`.
//!
//! Мутирующих операций три; запись выполняется только при явном `--apply`:
//!
//! - [`ops::edit`] меняет значения уже существующих полей уже существующих
//!   заметок;
//! - [`ops::create`] добавляет новые заметки в уже существующую колоду и модель.
//!   По умолчанию ссылки на медиа запрещены. Правила в `.anki-repo/create.yaml`
//!   могут разрешить только проверенные изображения кандзи для точных
//!   `crowdanki_uuid` и имени поля; произвольные ссылки на медиа по-прежнему
//!   запрещены. При таком разрешении `create` сначала размещает проверенные
//!   байты в `media/`, затем публикует изменения `media_files` и заметки в
//!   каноническом `deck.json` под общей блокировкой каталога экспорта. Если
//!   публикация JSON не удалась, уже размещённые проверенные файлы остаются для
//!   безопасного повторного использования;
//! - [`ops::retire`] дописывает тег вывода из обращения к тегам уже
//!   существующей заметки: физического удаления в toolkit'е нет вовсе.
//!
//! Только настроенный путь `create` может изменять `media/`; `edit` и `retire`
//! меняют только канонический `deck.json`. Ни одна команда не удаляет и не
//! перемещает сущности экспорта, не создаёт колоды, модели и конфигурации и не
//! меняет идентификаторы (`crowdanki_uuid`, `guid`, `note_model_uuid`).

pub mod cli;
pub mod error;
pub mod guid;
pub mod htmlscan;
pub mod index;
pub mod loader;
pub mod media;
pub mod model;
pub mod ops;
pub mod output;
pub mod paths;
pub mod proposal;
pub mod qa;
pub mod render;
pub mod report;
pub mod run;
pub mod selection;
pub mod template;
pub mod text;
pub mod write;

#[cfg(test)]
pub(crate) mod test_support;
