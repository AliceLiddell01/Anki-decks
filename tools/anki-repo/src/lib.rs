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
//! Мутирующих операций три, и каждая пишет только канонический `deck.json` по
//! явному `--apply`:
//!
//! - [`ops::edit`] меняет значения уже существующих полей уже существующих
//!   заметок;
//! - [`ops::create`] добавляет новые заметки в уже существующую колоду уже
//!   существующей модели: новым значениям запрещены ссылки на media, а колода и
//!   модель адресуются идентичностями, а не именами;
//! - [`ops::retire`] дописывает тег вывода из обращения к тегам уже
//!   существующей заметки: физического удаления в toolkit'е нет вовсе.
//!
//! Ни одна команда не изменяет `media/`, не удаляет и не перемещает сущности
//! экспорта, не создаёт колоды, модели и конфигурации и не меняет
//! идентификаторы (`crowdanki_uuid`, `guid`, `note_model_uuid`).

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
