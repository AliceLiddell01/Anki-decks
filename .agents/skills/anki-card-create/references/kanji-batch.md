# Kanji batch и ручная проверка

Эта инструкция применяется, когда фактическая `.anki-repo/create.yaml`
назначает `kanji_assets` полю создаваемой заметки. Публичные команды и их JSON
contract принадлежат `tools/asset-store/README.md`; здесь дан порядок
оркестрации. При расхождении проверь `kanji-assets --help` и owner README.
Не заменяй отсутствующий API ad hoc скриптом, который меняет manifest или
считает CV.

## Запуск и возобновление

Состоянием пакета и точными байтами кандидатов владеет `kanji-assets` под
`.asset-store/kanji/.runtime/batches/<batch-id>/`. Сохраняй batch ID из JSON
ответа вне чата, например в локальном workflow state вне `decks/**` и Git.
Повторный запуск с тем же ID возобновляет тот же requested identity set и
validator. ID уже существующего batch с другим набором identity будет отвергнут.

Если batch может изменить canonical corpus, до первого `batch start` запиши в
том же локальном workflow state push remote, default branch и точный baseline
SHA. Сверь canonical scope исходного checkout с этим SHA командами Git и
проверь store через `kanji-corpus-gate`. Сохрани вывод `kanji-assets list
--output json`, оставив в baseline только записи с `to_state=verified` и
`effective_status=verified`; это owner API, manifest вручную не читай. Этот
снимок и baseline SHA неизменны при resume. Если удалённый baseline сейчас
недоступен, batch всё равно можно продолжить, но без доказуемого asset-only
основания remote publication остаётся `LOCAL_READY_TO_PUBLISH`.
Если source store отсутствует и baseline содержит пустой corpus, owner-снимок
пуст. `list` в этом состоянии завершается ненулевым exit code и отдаёт
`outcome = blocked` с точным `error.code = "store_missing"`; пустым состоянием
считай только этот код, и только после проверки отсутствия store directory.
Любая другая ошибка `list` закрывает publication.

Команды ниже записаны в форме owner README
([`tools/asset-store/README.md`](../../../../tools/asset-store/README.md)):
отдельного бинарника в `PATH` нет, запускай их из корня checkout.

```bash
# Создать batch; при отсутствии --batch-id владелец выдаст ID.
cargo run --locked --bin kanji-assets -- --output json batch start 漢 字

# Пройти до пяти новых breadth-first rounds (по умолчанию — пять).
cargo run --locked --bin kanji-assets -- --output json batch run --batch-id <id>

# Увидеть persistent machine state и outcomes.
cargo run --locked --bin kanji-assets -- --output json batch status --batch-id <id>
```

`batch start` сначала переиспользует каждый asset, который owner подтверждает
как current effective verified. Один уже trusted asset не приобретает новые
bytes повторно. `batch run` выполняет весь текущий frontier до начала следующего
retry: unresolved identity второго раунда не опережает ещё не обработанные
identity первого. Item-level network/semantic failures не отменяют соседние
результаты. Повтор после process interruption перечитывает persistent state и
продолжает только следующий frontier.

Результат содержит `batch_id`, `counts`, `items`, `issues`, `blockers` и полный
`batch` state. Для item проверяй как минимум `state`, `candidate_sha256`,
`published_sha256`, `publication_source`, `acquisition_attempts`,
`distinct_valid_hashes` и `item_outcome`. Не определяй readiness только по общему
exit code: полное разрешение подтверждено `counts.effective_verified` и
`items[].effective_verified`; у semantic candidates должны совпадать
`candidate_sha256` и `published_sha256`.

`batch run --rounds N` допускает `1..=5` rounds за вызов; общий автоматический
лимит поколения — пять attempts на identity. Ошибка источника или технически
повреждённые bytes записываются в историю, но не становятся хорошим semantic
sample. Повтор того же exact SHA считается воспроизводимостью и не увеличивает
число независимых голосов. После user-directed reject/reacquire начинается новое
targeted generation только для затронутой identity; остальные ready assets
сохраняются.

## Проверка и решение пользователя

Для unresolved items после автоматического лимита создай визуальный artifact:

```bash
cargo run --locked --bin kanji-assets -- --output json batch review --batch-id <id>
```

Ответ содержит `review_artifact` — локальный HTML в ignored runtime. Передай его
пользователю вместе с кратким JSON контекстом. Проверяй character, current SHA,
format, automated status, attempts, distinct candidates, bounded per-attempt
evidence, aggregate mean/min/max/count и nearest competitor. Artifact показывает
все distinct candidate images; GIF сохраняет анимацию. Он не владеет решением:
перед действием снова прочитай `batch status`, чтобы исключить устаревший SHA.

Свободный русский ответ классифицируй только при однозначном match к identity и
показанному current SHA:

| Ясное решение пользователя | Structured action |
|---|---|
| `漢 - подтверждён` | `confirm` |
| `字 - артефакт битый` / «не тот иероглиф» | `reject` |
| Явная просьба получить другой candidate без оценки текущего | `reacquire` |
| Неоднозначный/неполный ответ | Не меняй state; уточни identity или действие |

Применяй ровно одно решение одной identity за вызов:

```bash
cargo run --locked --bin kanji-assets -- --output json batch decide \
  --batch-id <id> --character 漢 --sha256 <current-sha256> \
  --action confirm --reason "пользователь подтвердил текущий candidate"

cargo run --locked --bin kanji-assets -- --output json batch decide \
  --batch-id <id> --character 字 --sha256 <current-sha256> \
  --action reject --reason "пользователь указал, что это другой иероглиф"
```

`confirm`, `reject` и `reacquire` требуют exact текущий `--sha256` и явный
`--reason`. `confirm` не обходит image format/decode/integrity/path проверки;
owner отдельно сохраняет automated record и human attestation для того же
identity/hash. Human reject действует и против прежнего automated `VERIFIED`.
При изменившихся bytes старое решение отвергается. Human decision — semantic
решение пользователя, не криптографическая подпись.

После `confirm` проверь `batch status` и owner effective verified; если процесс
прервался между durable human intent и canonical publication, вызови `batch run`
с тем же ID — owner boundary продолжит publication по сохранённым exact bytes,
не повторяя browser acquisition. После `reject` запусти `batch run` только для
нового frontier; когда item останется unresolved — создай новую review версию.

Если пять технических failures не оставили candidate для решения, запусти
явный новый acquisition generation:

```bash
cargo run --locked --bin kanji-assets -- --output json batch retry --batch-id <id> \
  --character 字 --reason "источник снова доступен"
```

Не используй `retry` с candidate вместо exact decision: передай его SHA и
`--action reacquire` через `batch decide`.

## Безопасная asset-only публикация

Ветка создаётся только после terminal resolution всех requested identities.
Перед подготовкой публикации повторно вызови `kanji-assets list --output json`
и сравни canonical verified records по `identity`, `sha256`, effective status,
human decision и validation evidence с сохранённым owner-снимком. Каждая
изменившаяся identity должна входить в requested set этого batch, а её SHA и
effective trust должны совпадать с итоговым `batch status`. Если изменилась
посторонняя identity или source owner state нельзя согласовать с batch,
останови publication и сохрани `LOCAL_READY_TO_PUBLISH`; не копируй и не удаляй
эту delta и не запускай acquisition заново.

Создай отдельную ветку `kanji-assets/<batch-id>` в worktree от сохранённого
baseline SHA. Скопируй canonical corpus из source store, сохранив прежние
identity и exact trust records; исключи `.runtime`, `.tmp`, lock и любые
review/browser/acceptance artifacts. Сверь owner records и байты canonical
файлов source и worktree, затем stage только принадлежащие corpus пути,
которые реально существуют: `.owner.json`, `manifest.json`, `assets/*.gif`,
`assets/*.png` и необходимая canonical metadata. Выполни
`kanji-corpus-gate`; staged diff и worktree status должны содержать только
canonical asset corpus. При пустом delta не создавай ветку или commit.

Worktree стоит на baseline SHA, а его код умеет читать только тот manifest
schema, который существовал в baseline; corpus, записанный текущим кодом, может
иметь более новый `schema_version`. Поэтому gate запускай кодом **текущего**
checkout против worktree как CWD, а не бинарником из baseline worktree:

```bash
# Выполняется внутри worktree; <checkout> — корень текущего checkout.
cargo run --quiet --locked --manifest-path <checkout>/Cargo.toml \
  -p asset-store --bin kanji-corpus-gate
```

Если gate отвергает schema и текущий checkout её не понижает, publication
остаётся `LOCAL_READY_TO_PUBLISH`: не понижай `schema_version` вручную и не
переписывай manifest под старый reader.

После локального asset-only commit fetch фактическую актуальную base и
обнови unpublished ветку через `anki-git-workflow`, сохраняя изменения обеих
сторон. Не замещай manifest целиком стратегией `ours`/`theirs`. При неразрешимом
corpus conflict оставь branch, commit и batch state локально, запрети push и
покажи `LOCAL_READY_TO_PUBLISH`; acquisition не повторяй. После успешного
обновления снова выполни `kanji-corpus-gate` тем же способом (код текущего
checkout), проверь, что независимые base
identities сохранены, а diff относительно fetched base содержит только
canonical corpus и изменения identities текущего batch. Это локальное
обновление ветки, не merge PR; PR merge требует отдельной текущей команды
пользователя.

## Продвижение карточек

После каждого автоматического round, решения и reacquire снова прочитай `status`
и продвигай все notes, чьи **все** processor dependencies effective verified.
Только эти notes запускай через `anki-repo create` dry-run; учитывай полный
machine-readable plan и blockers, затем применяй лишь полностью разрешённые
notes. Независимые notes создавай, пока другие ожидают human review. Запросы,
resolved GUID и create outcomes сохраняй вне `decks/**`.

`visual-report` требует явные `--before` и `--after`; до первого `--apply`
сними состояние «до» в каталог вне `decks/**` (копия export или `git worktree`
закоммиченного состояния), иначе сравнивать будет не с чем. После записи запусти
`anki-repo validate` и сравни фактический export через `visual-report` между
этим снимком и текущим состоянием, а сам каталог отчёта держи вне `decks/**`.
Повтор flow должен дать no-op, а не duplicate note/media.

Подробная схема создания и частичного прогресса находится в родительском
[`SKILL.md`](../SKILL.md); contract команд — в
[`tools/asset-store/README.md`](../../../../tools/asset-store/README.md) и
[`tools/anki-repo/README.md`](../../../../tools/anki-repo/README.md).
