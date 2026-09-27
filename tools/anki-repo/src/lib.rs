//! `anki-repo` — toolkit для CrowdAnki-экспортов репозитория Anki-decks.
//!
//! Библиотечная часть отделена от executable boundary: тонкий [`crate::cli`]
//! и [`crate::run`] снаружи, но загрузка CrowdAnki, обход дерева, индексы,
//! domain-операции, validation и output contracts тестируются напрямую.
//!
//! `inspect`, `find`, `stats`, `validate`, `qa` и `review` только читают.
//! `review-check` тоже ничего не пишет: он проверяет proposals внешнего
//! reviewer'а и компилирует их в запрос для `edit`. Единственная мутирующая
//! операция — `edit`: она меняет значения уже существующих полей уже
//! существующих заметок и делает это только для канонического `deck.json`, по
//! явному `--apply` и после проверок, описанных в [`crate::ops::edit`]. Ни одна
//! команда не изменяет `media/` и не добавляет, не удаляет и не перемещает
//! сущности экспорта.

pub mod cli;
pub mod error;
pub mod index;
pub mod loader;
pub mod media;
pub mod model;
pub mod ops;
pub mod output;
pub mod proposal;
pub mod qa;
pub mod render;
pub mod run;
pub mod selection;
pub mod text;
pub mod write;

#[cfg(test)]
pub(crate) mod test_support;
