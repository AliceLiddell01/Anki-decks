//! Единственный владелец понятия «Anki-совместимый guid нового объекта».
//!
//! Почему это отдельный модуль, а не строчка рядом с местом использования:
//! guid — это значение, которое увидит Anki, и любая его копия где-то ещё
//! немедленно разойдётся с оригиналом. Ровно тот же формат обязан порождать и
//! генератор нового объекта, и проверка уже существующего значения, поэтому
//! здесь живут и кодирование, и валидация, а потребители не выводят формат
//! сами.
//!
//! Формат выбран не нами: guid — это `base91` от случайного `u64`, ровно так
//! его собирает [`anki_base91`] в `rslib/src/notes/mod.rs` при `Note::new`.
//! Совместимость здесь означает не «похоже на Anki», а «то же самое значение»:
//! любой другой алфавит или другой порядок цифр даст строку, которую Anki
//! примет как текст (схема БД объявляет `guid text NOT NULL` без ограничений и
//! без валидации формата), но которая перестанет быть тем, что генерирует
//! приложение. Поэтому алфавит и разворот цифр скопированы точно, а эталонные
//! векторы апстрима закреплены тестом.
//!
//! guid — не секрет и не уникальный ключ базы: он случаен, короток и
//! теоретически может совпасть с уже существующим. Anki не запрещает повторы
//! (колонка не уникальна, валидации формата нет), а CrowdAnki-экспорт требует
//! лишь уникальности внутри одного экспорта. Поэтому уникальность внутри
//! экспорта обеспечивает вызывающий код, а не [`generate`]: здесь мы отвечаем
//! только за формат и за то, что новый guid непуст.

use crate::details;
use crate::error::{DomainError, ErrorCode};

/// Максимальная длина guid, которую генерирует Anki.
///
/// `u64::MAX` требует десяти цифр base91, а меньшие значения дают меньше
/// символов; из этого же предела следует вместимость буфера при кодировании.
pub const MAX_GUID_CHARS: usize = 10;

/// Символы алфавита base91, из которых Anki собирает guid.
///
/// Порядок значим: индекс символа — это цифра в системе счисления по
/// основанию 91. Значение скопировано из апстрима без изменений.
pub const BASE91_ALPHABET: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!#$%&()*+,-./:;<=>?@[]^_`{|}~";

/// Основание системы счисления base91: длина [`BASE91_ALPHABET`].
const BASE91_RADIX: u64 = 91;

/// Кодирует `value` в base91-строку так же, как это делает Anki.
///
/// Функция чистая: одинаковый вход всегда даёт одинаковый выход. При
/// `value == 0` результат пуст — это ровно поведение апстрима, а не ошибка
/// кодирования; [`generate`] обязан такой результат не возвращать.
#[must_use]
pub fn base91(value: u64) -> String {
    let alphabet = BASE91_ALPHABET.as_bytes();
    let mut digits: Vec<u8> = Vec::with_capacity(MAX_GUID_CHARS);
    let mut remaining = value;

    // Цифры выходят от младшей к старшей, поэтому строка разворачивается.
    while remaining > 0 {
        let index = (remaining % BASE91_RADIX) as usize;
        // `index < BASE91_RADIX` по построению, а равенство радикса длине
        // алфавита закреплено тестом `alphabet_matches_radix`, поэтому ветка
        // запаса недостижима и нужна только для отсутствия паники.
        let byte = alphabet.get(index).copied().unwrap_or(b'a');
        digits.push(byte);
        remaining /= BASE91_RADIX;
    }

    digits.reverse();
    String::from_utf8_lossy(&digits).into_owned()
}

/// Генерирует guid нового объекта: 64 случайных бита, закодированные в base91.
///
/// Два качества для вызывающего кода важнее удобства: результат никогда не
/// пуст (пустая строка — это уже невалидный guid, а `value == 0` отбрасывается
/// и энтропия запрашивается снова) и любая проблема источника энтропии
/// возвращается как [`ErrorCode::Internal`], а не превращается в панику или в
/// пустой guid.
///
/// # Errors
///
/// [`ErrorCode::Internal`], если системный источник энтропии недоступен.
pub fn generate() -> Result<String, DomainError> {
    // Ограниченное число попыток: источник энтропии, который подряд отдаёт
    // нули, — это сломанное окружение, и бесконечный цикл скрыл бы это.
    const MAX_ATTEMPTS: usize = 64;

    for _ in 0..MAX_ATTEMPTS {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|error| {
            DomainError::with_details(
                ErrorCode::Internal,
                format!("не удалось получить энтропию для guid: {error}"),
                details! { "source" => "getrandom" },
            )
        })?;

        // Порядок байтов фиксирован: значение всё равно случайно, но фиксация
        // делает результат воспроизводимым при подмене источника энтропии.
        let value = u64::from_le_bytes(bytes);
        if value != 0 {
            return Ok(base91(value));
        }
    }

    Err(DomainError::with_details(
        ErrorCode::Internal,
        format!("источник энтропии вернул ноль {MAX_ATTEMPTS} раз подряд"),
        details! { "attempts" => MAX_ATTEMPTS },
    ))
}

/// Проверяет, что строка является Anki-совместимым guid.
///
/// Проверка намеренно строже схемы БД Anki: пустое значение отклоняется
/// (валидатор CrowdAnki-экспорта репозитория считает отсутствие guid ошибкой
/// `note_guid_missing`), длина ограничена [`MAX_GUID_CHARS`], а символы — только
/// из [`BASE91_ALPHABET`]. Поскольку алфавит состоит из ASCII, длина в байтах и
/// в символах совпадают, и проверка длины по байтам не может принять
/// многобайтовую строку.
#[must_use]
pub fn is_valid(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= MAX_GUID_CHARS
        && candidate
            .chars()
            .all(|symbol| BASE91_ALPHABET.contains(symbol))
}

/// Проверяет guid из внешнего источника, называя поле, в котором он пришёл.
///
/// Автор кода, читающего guid из запроса или экспорта, получает готовую
/// диагностику вместо булева флага: имя поля попадает и в сообщение, и в
/// машинные детали.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`], если [`is_valid`] вернула `false`.
pub fn validate(candidate: &str, field: &str) -> Result<(), DomainError> {
    if is_valid(candidate) {
        return Ok(());
    }

    Err(DomainError::with_details(
        ErrorCode::InvalidRequest,
        format!(
            "значение поля «{field}» не является Anki-совместимым guid: ожидается непустая строка не длиннее {MAX_GUID_CHARS} символов алфавита base91"
        ),
        details! {
            "field" => field,
            "length" => candidate.chars().count(),
            "max_chars" => MAX_GUID_CHARS,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_matches_radix() {
        assert_eq!(BASE91_ALPHABET.chars().count(), BASE91_RADIX as usize);
        assert_eq!(BASE91_ALPHABET.len(), BASE91_RADIX as usize);
        assert!(
            BASE91_ALPHABET.is_ascii(),
            "алфавит должен быть ASCII: иначе длина в байтах не равна длине в символах"
        );
    }

    #[test]
    fn base91_matches_upstream_vectors() {
        assert_eq!(base91(0), "");
        assert_eq!(base91(1), "b");
        assert_eq!(base91(1234567890), "saAKk");
        assert_eq!(base91(u64::MAX), "Rj&Z5m[>Zp");
    }

    #[test]
    fn base91_is_deterministic_and_bounded() {
        for value in [1_u64, 2, 90, 91, 92, 4096, 1_000_000_007, u64::MAX] {
            assert_eq!(
                base91(value),
                base91(value),
                "кодирование не детерминировано"
            );
            assert!(
                base91(value).chars().count() <= MAX_GUID_CHARS,
                "длина кодирования превысила предел"
            );
        }
    }

    #[test]
    fn base91_uses_only_alphabet_symbols() {
        for value in [0_u64, 1, 12345, 987_654_321, u64::MAX / 3] {
            let encoded = base91(value);
            assert!(
                encoded
                    .chars()
                    .all(|symbol| BASE91_ALPHABET.contains(symbol))
            );
        }
    }

    #[test]
    fn generated_guids_are_valid_and_distinct() {
        let mut seen: Vec<String> = Vec::with_capacity(256);
        for _ in 0..256 {
            let guid = generate().expect("системный источник энтропии должен быть доступен");
            assert!(
                !guid.is_empty(),
                "generate() не имеет права вернуть пустой guid"
            );
            assert!(
                is_valid(&guid),
                "guid {guid} не проходит собственную проверку"
            );
            assert!(!seen.contains(&guid), "guid {guid} повторился на выборке");
            seen.push(guid);
        }
        assert_eq!(seen.len(), 256);
    }

    #[test]
    fn empty_and_oversized_guids_are_rejected() {
        assert!(!is_valid(""));
        assert!(!is_valid(&"a".repeat(MAX_GUID_CHARS + 1)));
        assert!(is_valid(&"a".repeat(MAX_GUID_CHARS)));
    }

    #[test]
    fn characters_outside_alphabet_are_rejected() {
        assert!(!is_valid("абв"));
        assert!(!is_valid("a b"));
        assert!(!is_valid("a\"b"));
        assert!(!is_valid("a\\b"));
        assert!(!is_valid("a\u{200b}b"));
        assert!(is_valid("Rj&Z5m[>Zp"));
    }

    #[test]
    fn validate_uses_invalid_request_code() {
        let error =
            validate("кириллица", "guid").expect_err("значение вне алфавита обязано падать");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.code.as_str(), "invalid_request");
        assert!(error.message.contains("guid"));
        assert_eq!(error.details["field"], serde_json::json!("guid"));
        assert_eq!(
            error.details["max_chars"],
            serde_json::json!(MAX_GUID_CHARS)
        );

        assert!(validate("b", "guid").is_ok());
        assert_eq!(
            validate("", "note.guid").unwrap_err().details["field"],
            serde_json::json!("note.guid")
        );
    }
}
