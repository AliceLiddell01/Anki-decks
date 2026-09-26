//! `anki-repo` — read-only toolkit для CrowdAnki-экспортов репозитория
//! Anki-decks.
//!
//! Библиотечная часть отделена от executable boundary: тонкий [`crate::cli`]
//! и [`crate::run`] снаружи, но загрузка CrowdAnki, обход дерева, индексы,
//! domain-операции, validation и output contracts тестируются напрямую.
//!
//! Tool никогда не изменяет `deck.json`, `media/` или любые другие данные
//! репозитория.

pub mod cli;
pub mod error;
pub mod index;
pub mod loader;
pub mod media;
pub mod model;
pub mod ops;
pub mod render;
pub mod run;

#[cfg(test)]
pub(crate) mod test_support;
