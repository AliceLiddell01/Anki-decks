//! Диапазоны Unicode для identities предметной области кандзи.

/// Проверяет, входит ли символ в диапазоны CJK, используемые pinned builder'ом
/// KanjiVG.
pub fn is_supported_han(character: char) -> bool {
    matches!(
        u32::from(character),
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2FA1F
            | 0x30000..=0x3134F
    )
}

/// Разбирает один точный Unicode scalar из поддерживаемой области Han/CJK.
///
/// Identity сохраняется без изменений: функция не нормализует и не подменяет
/// Unicode characters.
pub fn parse_kanji_character(value: &str) -> Result<char, String> {
    let mut characters = value.chars();
    let character = characters.next().ok_or_else(|| {
        "параметр character должен содержать один символ поддерживаемой области Han/CJK".to_owned()
    })?;
    if characters.next().is_some() || !is_supported_han(character) {
        return Err(
            "параметр character должен содержать один символ поддерживаемой области Han/CJK".into(),
        );
    }
    Ok(character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_han_ranges_accept_representative_scalars() {
        for character in ['㐀', '一', '豈', '𠀀', '𰀀'] {
            assert!(is_supported_han(character), "символ {character}");
            assert_eq!(parse_kanji_character(&character.to_string()), Ok(character));
        }
    }

    #[test]
    fn unsupported_unicode_domains_are_rejected() {
        for value in ["A", "あ", "😀", "\n", "漢字", ""] {
            assert!(parse_kanji_character(value).is_err(), "ввод {value:?}");
        }
    }

    #[test]
    fn supported_ranges_have_stable_boundaries() {
        for character in ['㐀', '䶿', '一', '鿿', '豈', '﫿', '𠀀', '𯨟', '𰀀', '𱍏']
        {
            assert!(
                is_supported_han(character),
                "U+{:04X}",
                u32::from(character)
            );
        }
        for codepoint in [0x33FF, 0x4DC0, 0xA000, 0xFB00, 0x1F600, 0x2FA20, 0x31350] {
            let character = char::from_u32(codepoint).expect("кодовая точка является scalar");
            assert!(
                !is_supported_han(character),
                "U+{:04X}",
                u32::from(character)
            );
        }
    }
}
