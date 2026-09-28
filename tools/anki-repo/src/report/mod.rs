//! Детерминированный отчёт: сравнение двух состояний одного логического экспорта.
//!
//! Разделение внутри модуля:
//!
//! - [`diff`] — единственный владелец сравнения значений полей;
//! - [`html`] — единственный владелец HTML-представления отчёта;
//! - [`style`] — статические стили, чтобы HTML собирался без внешних ссылок;
//! - [`media`] — безопасное копирование media-файлов в каталог отчёта;
//! - [`runtime`] — собственный JavaScript отчёта и его граница доверия.

pub mod diff;
pub mod html;
pub mod manifest;
pub mod media;
pub mod runtime;
pub mod sanitize;
pub mod style;

/// Состояние экспорта, к которому относится превью.
///
/// Состояние — это часть личности превью, а не подпись: превью «до» строится из
/// заметки и модели состояния «до», превью «после» — из состояния «после», и
/// media каждого состояния берётся из своего экспорта. Иначе одинаковое имя
/// файла в обоих состояниях показало бы «до» содержимым из «после».
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SideState {
    /// Состояние «до».
    Before,
    /// Состояние «после».
    After,
}

impl SideState {
    /// Оба состояния в порядке отчёта.
    pub const ALL: [Self; 2] = [Self::Before, Self::After];

    /// Стабильное машинное имя состояния.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }

    /// Подпись состояния в отчёте.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Before => "До",
            Self::After => "После",
        }
    }

    /// Подкаталог media внутри каталога отчёта.
    ///
    /// Состояния не делят один каталог: одинаковое имя файла в обоих состояниях
    /// может содержать разное содержимое, и тогда общий путь молча показал бы
    /// файл не из своего состояния.
    #[must_use]
    pub const fn media_subdir(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }

    /// Относительный каталог media внутри каталога отчёта.
    #[must_use]
    pub fn media_dir(self) -> String {
        format!("{}/{}", media::MEDIA_SUBDIR, self.media_subdir())
    }
}
