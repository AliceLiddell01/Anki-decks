//! Domain-операции: `inspect`, `find`, `stats`, `validate`.
//!
//! Каждая операция возвращает собственный domain result. Human и JSON
//! renderers — только два представления одного и того же результата.

pub mod find;
pub mod inspect;
pub mod stats;
pub mod validate;

use crate::media::MediaReport;

/// Именованное значение поля заметки в порядке `ord` модели.
#[derive(Debug, Clone)]
pub struct NamedField {
    /// Имя поля из определения модели.
    pub name: String,
    /// Позиция поля; `None`, если `ord` в модели malformed.
    pub ord: Option<i64>,
    /// Значение поля; `None`, если значения по этой позиции нет.
    pub value: Option<String>,
}

/// Пара «ключ — количество» для детерминированных распределений.
#[derive(Debug, Clone, PartialEq)]
pub struct CountEntry {
    /// Ключ распределения.
    pub key: String,
    /// Количество.
    pub count: usize,
}

/// Компактные media-счётчики.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaCounters {
    /// Сколько имён объявлено суммарно.
    pub declared_total: usize,
    /// Сколько уникальных имён объявлено.
    pub declared_unique: usize,
    /// Сколько объявленных имён повторяются.
    pub duplicate_declared: usize,
    /// Существует ли каталог `media/`.
    pub dir_present: bool,
    /// Сколько файлов физически лежит в `media/`.
    pub physical_total: usize,
    /// Сколько объявленных имён не найдено физически.
    pub missing_physical: usize,
    /// Сколько физических файлов не объявлено.
    pub undeclared_physical: usize,
}

impl MediaCounters {
    /// Считает счётчики по подробной сводке.
    pub fn from_report(report: &MediaReport) -> Self {
        Self {
            declared_total: report.declared_total,
            declared_unique: report.declared.len(),
            duplicate_declared: report.duplicate_declared.len(),
            dir_present: report.dir_present,
            physical_total: report.physical.len(),
            missing_physical: report.declared.difference(&report.physical).count(),
            undeclared_physical: report.physical.difference(&report.declared).count(),
        }
    }
}

/// Ограничение размера диагностических выборок в verbose-выводе.
pub const SAMPLE_LIMIT: usize = 5;
