# Pitch-accent batch и ручная проверка

Эта инструкция применяется, когда фактическая `.anki-repo/create.yaml`
назначает `pitch_accent` полю создаваемой заметки. Публичные команды и их JSON
contract принадлежат `tools/asset-store/README.md`; здесь дан порядок
оркестрации. При расхождении проверь `pitch-assets --help` и owner README.
Не заменяй отсутствующий API ad hoc скриптом, который меняет manifest или
самостоятельно рисует/сравнивает графики.

## Зависимости

Identity pitch-ресурса — точный `surface` слова, поэтому canonical имя файла
остаётся surface-based независимо от чтения. Извлекай surfaces только из полей,
которым policy назначила `pitch_accent`, и дедуплицируй их между notes. Чтение
(`reading`) повышает точность resolver'а и передаётся отдельно; оно не входит ни
в identity, ни в имя файла.

Хранилище по умолчанию — `.asset-store/pitch-accent` в корне checkout; `create`
читает его офлайн и никогда не ходит в сеть. Acquisition выполняет только
`pitch-assets`.

## Запуск и возобновление

Состоянием пакета владеет `pitch-assets` под
`.asset-store/pitch-accent/.runtime/batches/<batch-id>/`. Сохраняй batch ID в
локальном workflow state вне `decks/**` и Git. Повторный запуск с тем же ID
возобновляет тот же requested set; тот же ID с другим планом отвергается
(`identity_conflict`).

План — обычный JSON, а не второй источник identity:

```json
{
  "schema_version": 1,
  "items": [
    { "surface": "幽霊", "reading": "ゆうれい" },
    { "surface": "飴" }
  ]
}
```

```bash
# Создать пакет из плана; при отсутствии --batch-id владелец выдаст ID.
cargo run --locked --bin pitch-assets -- --output json batch start \
  --batch-id <id> --plan <plan.json>

# Пройти текущий frontier.
cargo run --locked --bin pitch-assets -- --output json batch run --batch-id <id>

# Продолжить прерванный прогон тем же код-путём.
cargo run --locked --bin pitch-assets -- --output json batch resume --batch-id <id>

# Прочитать persistent machine state без сети и браузера.
cargo run --locked --bin pitch-assets -- --output json batch status --batch-id <id>
```

`batch status` не ходит в сеть и не запускает браузер — это единственный
безопасный способ перечитать состояние перед действием.

**Сессия браузера одна на хост.** Acquisition kanji и pitch используют общий
runner-каталог браузера, поэтому одновременный запуск `kanji-assets batch run`
и `pitch-assets batch run` роняет вторую сессию на `browser_setup`
(`SingletonLock: File exists`). Выполняй браузерные acquisition последовательно,
а не параллельно; конфликт — это retryable техническая ошибка, а не отказ
источника.

## Исходы и что с ними делать

Разрешённые (`is_resolved`, exit `0`): `no_pitch_accent_on_source`,
`published`, `existing_verified`.

| Статус | Что делать |
|---|---|
| `pending` | `batch run` или `batch resume` |
| `acquired_verified`, `publication_pending` | следующий `run`/`resume` завершит публикацию |
| `existing_verified`, `published` | готово |
| `no_pitch_accent_on_source` | готово: у слова действительно нет pitch на источнике |
| `ambiguous_vocabulary` | `batch select` с явным выбором, затем `run` |
| `vocabulary_not_found` | **не** доказательство отсутствия pitch: замени слово |
| отказ `create` по занятому имени файла | legacy-конфликт имён, а не отсутствующая зависимость: см. «Canonical имя и legacy-конфликт» ниже |
| `technical_failure` | `batch retry`, только если исход retryable |
| `candidate_rejected` | `batch reacquire`, затем `run` |
| `conflict` | сначала `batch status`, затем `batch reacquire` с `--reason` |

`no_pitch_accent_on_source` — полноценная разрешённая зависимость: пустая запись
и placeholder media не создаются, а поле карточки остаётся пустым. Это не ошибка
и не повод менять слово.

`vocabulary_not_found` не означает отсутствие pitch. Не подставляй пустое поле по
этому исходу: замени test word или передай решение пользователю.

`ambiguous_vocabulary` никогда не разрешай автоматическим выбором первого
candidate. Передавай `reading`, если он объективно известен; если после свежего
search выбор всё ещё неоднозначен, запроси решение человека. Механизм выбора —
только owner API, который сверяет `vocabulary_id` и `detail_url` со свежим
списком кандидатов:

```bash
cargo run --locked --bin pitch-assets -- --output json batch select \
  --batch-id <id> --surface 重複語 --vocabulary-id <id> \
  --detail-url 'https://jpdb.io/vocabulary/<id>/重複語/ちょうふくご'
```

Перед точечными командами создай review artifact и передай его пользователю:

```bash
cargo run --locked --bin pitch-assets -- --output json batch review --batch-id <id>
```

Ответ содержит `artifact` — локальный HTML в ignored runtime. `batch retry`
допустим только для retryable технической ошибки; при неретраибельном исходе
владелец вернёт `invalid_transition`. `batch reject` требует точный текущий
`--sha256` из истории кандидатов пакета.

## Доверие

Pitch-домен считает ресурс пригодным для карточки только при current automated
`VERIFIED` от ожидаемого валидатора для exact SHA. Подтверждение человека не
заменяет и не переписывает это решение, поэтому «этот график выглядит верно» не
делает неverified кандидат пригодным: нужен новый acquisition generation
(`batch reacquire`) с текущим валидатором. Это отличается от `kanji_assets`, где
явное решение человека по exact bytes даёт effective trust.

`create` читает только canonical verified records; `REJECTED`, `UNCERTAIN`,
устаревшее evidence другой версии валидатора и незавершённая публикация для него
не ресурс. Если `create` возвращает `pitch_asset_missing` или
`asset_integrity_invalid`, сверь `batch status` и `corpus check`, а не подменяй
ссылку вручную.

## Canonical имя и legacy-конфликт

Canonical имя pitch-файла — `<surface>.pitch.png`, и оно остаётся таким всегда.
Прежний конвейер называл файл `<surface>.png`; для односложного слова с
kanji-fallback это имя совпадает с canonical именем изображения символа, и под
одним именем в плоском `media/` оказываются два разных файла. `create` в этом
случае отказывает (`destination_media_conflict`), а не выбирает, чьи байты важнее.

Это разрешено **не** переименованием файла и не правкой `deck.json` руками, а
командой `anki-repo migrate-media`, которая доказывает семантику каждой живой
ссылки и переводит её на canonical имя. Порядок, доказательства и итоговый отчёт —
в [SKILL.md](../SKILL.md), раздел «Legacy consumer filename»; контракт команды —
в `tools/anki-repo/README.md`. Не заканчивай набор на N−1 заметках и не заменяй
слово из-за этого отказа: конфликт ожидаем и разрешается миграцией.

## Продвижение карточек

После каждого раунда снова прочитай `batch status` и продвигай notes, у которых
**все** processor dependencies разрешены. Разрешённые `no_pitch_accent_on_source`
не блокируют создание: их поле остаётся пустым. Notes с неразрешённой pitch
зависимостью остаются pending без placeholder media; остальные notes создавай,
не дожидаясь ручного решения по ним.

## Публикация canonical pitch corpus

Pitch-корпус публикуется тем же порядком, что и kanji-корпус, отдельной
asset-only веткой от сохранённого baseline SHA, и только после terminal
resolution requested identities. Stage только принадлежащие corpus пути:
`.owner.json`, `manifest.json`, `assets/png/*.png`; `.runtime`, `.tmp`, lock,
review и browser artifacts в коммит не попадают. Gate — `pitch-corpus-gate`
(код текущего checkout, CWD — worktree). Публикацию и её admin gate владеет
родительский [`SKILL.md`](../SKILL.md); не создавай для pitch второй процедуры.

## Продвижение и проверка

`visual-report` требует явные `--before` и `--after`; до первого `--apply`
сними состояние «до» в каталог вне `decks/**`. После записи запусти
`anki-repo validate` и посмотри карточки в отчёте: pitch PNG читается на тёмном
фоне, `Ударение` либо ссылается на `<surface>.pitch.png`, либо пусто, broken
media отсутствуют, а `<surface>.png` и `<surface>.pitch.png` не подменяют друг
друга. Каталог отчёта держи вне `decks/**`. Повтор flow должен дать no-op.

Contract команд — в
[`tools/asset-store/README.md`](../../../../tools/asset-store/README.md) и
[`tools/anki-repo/README.md`](../../../../tools/anki-repo/README.md).
