//! Детерминированный token-level diff значений полей для визуального отчёта.
//!
//! Почему не сравнение отрендеренного текста: заметная часть правок в этом
//! репозитории — это правки HTML-разметки, а не слов. `<b>слово</b>` и
//! `<i>слово</i>` дают одинаковый plain text, поэтому сравнение по тексту
//! показало бы «изменений нет» ровно там, где ревьюеру нужно увидеть разницу.
//! Поэтому единица сравнения — токен: HTML-тег, HTML-сущность, пробельная серия,
//! серия ASCII-алфанумерики или отдельный символ.
//!
//! Почему LCS, а не «умное» выравнивание: отчёт обязан быть воспроизводимым.
//! Классический LCS с зафиксированным правилом разрешения ничьих даёт один и
//! тот же результат на одном и том же входе при любом запуске и на любой
//! платформе; никакой эвристики, зависящей от порядка обхода хеш-таблиц, здесь
//! нет. Цена — O(n·m) по времени и памяти, поэтому точный diff считается только
//! под лимитом [`MAX_DIFF_TOKENS`], а за лимитом отчёт честно деградирует до
//! грубой замены вместо тихой потери деталей.

/// Вид токена в результате сравнения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Токен присутствует в обеих версиях.
    Equal,
    /// Токен появился только в новой версии.
    Inserted,
    /// Токен присутствовал только в старой версии.
    Deleted,
}

/// Один фрагмент результата сравнения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffToken {
    /// Вид фрагмента.
    pub kind: TokenKind,
    /// Текст фрагмента; соседние фрагменты одного вида уже склеены.
    pub text: String,
}

/// Максимум токенов на сторону, для которых считается точный diff.
///
/// Предел ограничивает и время, и память: таблица длин имеет размер
/// `(n + 1) × (m + 1)` элементов, а длина любой выровненной подпоследовательности
/// не превышает `MAX_DIFF_TOKENS`, поэтому счётчики помещаются в `u16`.
pub const MAX_DIFF_TOKENS: usize = 4000;

/// Максимальная длина HTML-тега в байтах, при которой `<…>` считается тегом.
///
/// Слишком длинное `<…>` почти наверняка не тег, а текстовый символ «меньше»:
/// без предела одна незакрытая угловая скобка съела бы весь остаток значения.
const MAX_TAG_BYTES: usize = 512;

/// Максимальная длина HTML-сущности в байтах (`&` … `;`).
const MAX_ENTITY_BYTES: usize = 32;

/// Разбирает текст на токены, сохраняя исходные срезы без копирования.
///
/// Возвращаются именно срезы `&str`: токенизатор ничего не преобразует и не
/// нормализует, а все токены по построению покрывают вход целиком и идут в
/// порядке появления, поэтому владение строкой не нужно.
///
/// Правило токенизации (первое подходящее):
///
/// 1. `<` … `>` — HTML-тег, если `>` встречается не дальше [`MAX_TAG_BYTES`];
///    иначе `<` — отдельный символ;
/// 2. `&` … `;` — HTML-сущность, если между ними только ASCII-алфанумерика и
///    `#` и не дальше [`MAX_ENTITY_BYTES`]; иначе `&` — отдельный символ;
/// 3. серия пробельных символов;
/// 4. серия ASCII-алфанумерики;
/// 5. иначе — один символ (`char`), включая кириллицу и CJK.
///
/// Следствие пунктов 4 и 5: латинские слова сравниваются пословно (правка
/// внутри слова видна целиком), а кириллица и CJK — посимвольно, потому что
/// «словом» для них является всё значение.
#[must_use]
pub fn tokenize(text: &str) -> Vec<&str> {
    let mut tokens: Vec<&str> = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        let length = next_token_len(rest);
        let (token, tail) = rest.split_at(length);
        tokens.push(token);
        rest = tail;
    }

    tokens
}

/// Точный детерминированный diff; `None`, если сторона длиннее [`MAX_DIFF_TOKENS`].
///
/// Ничьи разрешаются в пользу удаления: если длины общих подпоследовательностей
/// при удалении и при вставке равны, сначала выводится удалённый токен. Правило
/// не «правильное» и не «красивое» — оно фиксированное, и это единственное, что
/// здесь требуется.
///
/// Одинаковые строки дают единственный [`TokenKind::Equal`] со всем текстом;
/// одинаковые пустые строки дают пустой результат, потому что склеивать нечего.
#[must_use]
pub fn diff_tokens(before: &str, after: &str) -> Option<Vec<DiffToken>> {
    let before_tokens = tokenize(before);
    let after_tokens = tokenize(after);
    if before_tokens.len() > MAX_DIFF_TOKENS || after_tokens.len() > MAX_DIFF_TOKENS {
        return None;
    }

    let rows = before_tokens.len();
    let columns = after_tokens.len();
    let width = columns + 1;

    // table[i * width + j] — длина LCS суффиксов before[i..] и after[j..].
    let mut table = vec![0_u16; (rows + 1) * width];
    for i in (0..rows).rev() {
        for j in (0..columns).rev() {
            let length = if before_tokens[i] == after_tokens[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
            table[i * width + j] = length;
        }
    }

    let mut steps: Vec<(TokenKind, &str)> = Vec::with_capacity(rows + columns);
    let (mut i, mut j) = (0, 0);
    while i < rows && j < columns {
        if before_tokens[i] == after_tokens[j] {
            steps.push((TokenKind::Equal, before_tokens[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            steps.push((TokenKind::Deleted, before_tokens[i]));
            i += 1;
        } else {
            steps.push((TokenKind::Inserted, after_tokens[j]));
            j += 1;
        }
    }
    while i < rows {
        steps.push((TokenKind::Deleted, before_tokens[i]));
        i += 1;
    }
    while j < columns {
        steps.push((TokenKind::Inserted, after_tokens[j]));
        j += 1;
    }

    Some(merge(steps))
}

/// Diff, который никогда не возвращает `None`.
///
/// При превышении лимита отдаётся грубая замена целиком: старое значение как
/// [`TokenKind::Deleted`], новое как [`TokenKind::Inserted`]. Ревьюер видит
/// «здесь сравнивать не стали», а не пустой отчёт.
#[must_use]
pub fn diff_tokens_lossy(before: &str, after: &str) -> Vec<DiffToken> {
    if let Some(tokens) = diff_tokens(before, after) {
        return tokens;
    }

    if before == after {
        return if before.is_empty() {
            Vec::new()
        } else {
            vec![DiffToken {
                kind: TokenKind::Equal,
                text: before.to_string(),
            }]
        };
    }

    let mut tokens: Vec<DiffToken> = Vec::with_capacity(2);
    if !before.is_empty() {
        tokens.push(DiffToken {
            kind: TokenKind::Deleted,
            text: before.to_string(),
        });
    }
    if !after.is_empty() {
        tokens.push(DiffToken {
            kind: TokenKind::Inserted,
            text: after.to_string(),
        });
    }
    tokens
}

/// Есть ли различия между значениями.
#[must_use]
pub fn has_changes(before: &str, after: &str) -> bool {
    before != after
}

/// Длина токена, начинающегося в начале `rest`; `rest` непуст.
fn next_token_len(rest: &str) -> usize {
    let first = rest.chars().next().expect("rest проверен на непустоту");

    if first == '<'
        && let Some(length) = tag_len(rest)
    {
        return length;
    }
    if first == '&'
        && let Some(length) = entity_len(rest)
    {
        return length;
    }
    if first.is_whitespace() {
        return run_len(rest, char::is_whitespace);
    }
    if first.is_ascii_alphanumeric() {
        return run_len(rest, |symbol| symbol.is_ascii_alphanumeric());
    }

    first.len_utf8()
}

/// Длина HTML-тега вместе с угловыми скобками, если он есть.
///
/// Поиск ограничен [`MAX_TAG_BYTES`] и не идёт по всему остатку: иначе в длинном
/// тексте без `>` каждый `<` просматривал бы текст до конца, и разбор был бы
/// квадратичным по длине значения.
fn tag_len(text: &str) -> Option<usize> {
    let end = text
        .as_bytes()
        .iter()
        .take(MAX_TAG_BYTES)
        .position(|byte| *byte == b'>')?;
    Some(end + 1)
}

/// Длина HTML-сущности вместе с `&` и `;`, если она есть.
fn entity_len(text: &str) -> Option<usize> {
    let end = text
        .as_bytes()
        .iter()
        .take(MAX_ENTITY_BYTES)
        .position(|byte| *byte == b';')?;
    let body = &text[1..end];
    if body.is_empty()
        || !body
            .chars()
            .all(|symbol| symbol == '#' || symbol.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(end + 1)
}

/// Длина самого длинного префикса, все символы которого удовлетворяют условию.
fn run_len(text: &str, predicate: impl Fn(char) -> bool) -> usize {
    let mut length = 0;
    for (offset, symbol) in text.char_indices() {
        if !predicate(symbol) {
            break;
        }
        length = offset + symbol.len_utf8();
    }
    length
}

/// Склеивает соседние фрагменты одного вида, чтобы отчёт не тонул в шуме.
fn merge(steps: Vec<(TokenKind, &str)>) -> Vec<DiffToken> {
    let mut tokens: Vec<DiffToken> = Vec::new();
    for (kind, text) in steps {
        match tokens.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(text),
            _ => tokens.push(DiffToken {
                kind,
                text: text.to_string(),
            }),
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(tokens: &[DiffToken]) -> Vec<TokenKind> {
        tokens.iter().map(|token| token.kind).collect()
    }

    fn compact(tokens: &[DiffToken]) -> Vec<(TokenKind, &str)> {
        tokens
            .iter()
            .map(|token| (token.kind, token.text.as_str()))
            .collect()
    }

    #[test]
    fn tokenizer_splits_by_documented_rule() {
        assert_eq!(
            tokenize("<b>слово</b>"),
            vec!["<b>", "с", "л", "о", "в", "о", "</b>"]
        );
        assert_eq!(tokenize("a1 b2"), vec!["a1", " ", "b2"]);
        assert_eq!(tokenize("水 &amp;"), vec!["水", " ", "&amp;"]);
        assert_eq!(tokenize("  \t\n"), vec!["  \t\n"]);
        assert_eq!(tokenize(""), Vec::<&str>::new());
        assert_eq!(tokenize("a < b"), vec!["a", " ", "<", " ", "b"]);
        assert_eq!(tokenize("&#39;"), vec!["&#39;"]);
        assert_eq!(tokenize("&no-semicolon"), vec!["&", "no", "-", "semicolon"]);
    }

    #[test]
    fn tokenizer_keeps_unterminated_markup_as_characters() {
        assert_eq!(tokenize("<div"), vec!["<", "div"]);
        assert_eq!(tokenize("&"), vec!["&"]);
        let long_tag = format!("<{}>", "a".repeat(MAX_TAG_BYTES));
        assert_eq!(tokenize(&long_tag)[0], "<");
    }

    #[test]
    fn identical_text_gives_single_equal_token() {
        let tokens = diff_tokens("salt and pepper", "salt and pepper").expect("в пределах лимита");
        assert_eq!(
            compact(&tokens),
            vec![(TokenKind::Equal, "salt and pepper")]
        );
        assert!(!has_changes("salt and pepper", "salt and pepper"));
    }

    #[test]
    fn identical_empty_texts_give_no_tokens() {
        assert_eq!(diff_tokens("", ""), Some(Vec::new()));
        assert_eq!(diff_tokens_lossy("", ""), Vec::new());
        assert!(!has_changes("", ""));
    }

    #[test]
    fn word_replacement_is_reported_around_shared_prefix() {
        let tokens = diff_tokens("salt and pepper", "salt and sugar").expect("в пределах лимита");
        assert_eq!(
            compact(&tokens),
            vec![
                (TokenKind::Equal, "salt and "),
                (TokenKind::Deleted, "pepper"),
                (TokenKind::Inserted, "sugar"),
            ]
        );
        assert!(has_changes("salt and pepper", "salt and sugar"));
    }

    #[test]
    fn html_markup_change_is_visible() {
        let tokens = diff_tokens("<b>значение</b>", "<i>значение</i>").expect("в пределах лимита");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Deleted && token.text == "<b>")
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Inserted && token.text == "<i>")
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Equal && token.text == "значение")
        );
    }

    #[test]
    fn html_entities_are_compared_as_whole_tokens() {
        let tokens = diff_tokens("a &amp; b", "a &lt; b").expect("в пределах лимита");
        assert_eq!(
            compact(&tokens),
            vec![
                (TokenKind::Equal, "a "),
                (TokenKind::Deleted, "&amp;"),
                (TokenKind::Inserted, "&lt;"),
                (TokenKind::Equal, " b"),
            ]
        );
    }

    #[test]
    fn cyrillic_and_cjk_are_compared_per_character() {
        let tokens = diff_tokens("水分", "水気").expect("в пределах лимита");
        assert_eq!(
            compact(&tokens),
            vec![
                (TokenKind::Equal, "水"),
                (TokenKind::Deleted, "分"),
                (TokenKind::Inserted, "気"),
            ]
        );

        let tokens = diff_tokens("привет", "приют").expect("в пределах лимита");
        assert_eq!(
            compact(&tokens),
            vec![
                (TokenKind::Equal, "при"),
                (TokenKind::Deleted, "ве"),
                (TokenKind::Inserted, "ю"),
                (TokenKind::Equal, "т"),
            ]
        );
    }

    #[test]
    fn insertion_and_deletion_at_edges_are_kept() {
        assert_eq!(
            diff_tokens("", "x"),
            Some(vec![DiffToken {
                kind: TokenKind::Inserted,
                text: "x".to_string(),
            }])
        );
        assert_eq!(
            diff_tokens("x", ""),
            Some(vec![DiffToken {
                kind: TokenKind::Deleted,
                text: "x".to_string(),
            }])
        );
    }

    #[test]
    fn adjacent_tokens_of_one_kind_are_merged() {
        let tokens = diff_tokens("a b c", "a").expect("в пределах лимита");
        assert_eq!(kinds(&tokens), vec![TokenKind::Equal, TokenKind::Deleted]);
        assert_eq!(tokens[0].text, "a");
        assert_eq!(tokens[1].text, " b c");
    }

    /// Полностью заменённый фрагмент остаётся честным LCS-выравниванием, а не
    /// «грубой заменой»: совпавшие пробелы не выдумываются заново.
    #[test]
    fn complete_replacement_stays_lcs_aligned() {
        let tokens = diff_tokens("a b c", "x y z").expect("в пределах лимита");
        assert_eq!(
            compact(&tokens),
            vec![
                (TokenKind::Deleted, "a"),
                (TokenKind::Inserted, "x"),
                (TokenKind::Equal, " "),
                (TokenKind::Deleted, "b"),
                (TokenKind::Inserted, "y"),
                (TokenKind::Equal, " "),
                (TokenKind::Deleted, "c"),
                (TokenKind::Inserted, "z"),
            ]
        );
    }

    /// Инвариант для любого входа: результат восстанавливает обе стороны.
    #[test]
    fn diff_reconstructs_both_sides() {
        let pairs = [
            ("", ""),
            ("a", "b"),
            ("salt and pepper", "salt and sugar"),
            ("<b>水</b>", "<i>水</i>"),
            ("&amp;", "&lt;"),
            ("abc", "abcdef"),
            ("abcdef", "abc"),
        ];

        for (before, after) in pairs {
            let tokens = diff_tokens(before, after).expect("в пределах лимита");
            let rebuilt_before: String = tokens
                .iter()
                .filter(|token| token.kind != TokenKind::Inserted)
                .map(|token| token.text.as_str())
                .collect();
            let rebuilt_after: String = tokens
                .iter()
                .filter(|token| token.kind != TokenKind::Deleted)
                .map(|token| token.text.as_str())
                .collect();
            assert_eq!(rebuilt_before, before, "левая сторона не восстановлена");
            assert_eq!(rebuilt_after, after, "правая сторона не восстановлена");
        }
    }

    #[test]
    fn diff_is_deterministic() {
        let before = "пример <b>значения</b> с текстом";
        let after = "пример <i>значения</i> без текста";
        let first = diff_tokens(before, after).expect("в пределах лимита");
        let second = diff_tokens(before, after).expect("в пределах лимита");
        assert_eq!(first, second);
        assert_eq!(diff_tokens_lossy(before, after), first);
    }

    #[test]
    fn oversized_side_is_refused_by_exact_diff() {
        let long = "水".repeat(MAX_DIFF_TOKENS + 1);
        assert_eq!(tokenize(&long).len(), MAX_DIFF_TOKENS + 1);
        assert!(diff_tokens(&long, "короткое").is_none());
        assert!(diff_tokens("короткое", &long).is_none());

        let lossy = diff_tokens_lossy(&long, "короткое");
        assert_eq!(
            compact(&lossy),
            vec![
                (TokenKind::Deleted, long.as_str()),
                (TokenKind::Inserted, "короткое")
            ]
        );

        let lossy = diff_tokens_lossy(&long, &long);
        assert_eq!(compact(&lossy), vec![(TokenKind::Equal, long.as_str())]);
    }

    #[test]
    fn exact_limit_still_gives_exact_diff() {
        let exactly = "水".repeat(MAX_DIFF_TOKENS);
        let tokens = diff_tokens(&exactly, &exactly).expect("ровно лимит допустим");
        assert_eq!(compact(&tokens), vec![(TokenKind::Equal, exactly.as_str())]);
    }
}
