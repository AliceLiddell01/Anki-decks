# Контроль дискового мусора

`repository-maintenance` — отдельный Cargo crate для наблюдения за локальными
артефактами и безопасной очистки воспроизводимых данных. Он отделён от
`anki-repo`, потому что не работает с CrowdAnki-экспортами и отвечает за
жизненный цикл диска.

## Что нашёл аудит

На текущем `HEAD` источники распределены так:

| Область | Фактический источник | Политика |
|---|---|---|
| Cargo workspace | Единственный корневой workspace; обычный `target` определяется через `cargo metadata`. Cargo build/test profiles объявлены в корневом `Cargo.toml`. | Внутри workspace измеряется вместе с настроенным Cargo `build-dir`; очистка только через `cargo clean`. |
| Вложенные сборки | `anki-repo` code-review запускает Cargo Clippy; синтетические Cargo-контракты задают `CARGO_TARGET_DIR` внутри своего `TempWorkspace`. | Временный каталог удаляется владельцем или подбирается существующим orphan GC. |
| Временные worker/browser/test данные | `asset-store` создаёт `TempWorkspace` с приватным ownership marker; браузер получает отдельные профиль и `TMPDIR` внутри этого дерева. | Используются process identity, PID start time, ссылки процессов, `NOFOLLOW` и mount boundary из `asset-store`. Этот инструмент вызывает те же публичные GC-пути. |
| ZIP с evidence | `tools/asset-store/src/bin/common/evidence_zip.rs` сохраняет архив в `<temp_dir>/anki-decks-evidence` либо в путь `--output`. | ZIP — сохраняемое evidence, поэтому новый GC только показывает его в инвентаризации. Каталог и файлы без проверяемого owner marker не удаляются. Явный `--output` вне `/tmp` не входит в просмотр. |
| Runtime store и транзакционные `.tmp` | Данные asset store расположены в его управляемом каталоге; временные имена используются атомарными операциями и восстановлением незавершённых переходов. | Это состояние домена, а не общий Cargo мусор; disk GC его не очищает. |
| Постоянные внешние Cargo targets | В текущем исходном коде отдельного долговечного producer'а под `~/.cache` не найдено. Случайные каталоги из локального инцидента не являются доказательством владения. | Для будущих/явных reusable caches есть `cache init` и `cache run`: они создают marker, обновляют last-used и подпадают под общий лимит. Неизвестные каталоги не очищаются. |

Профили Cargo проверены без изменения. `[profile.test]` и `[profile.ci]` уже
используют `debug = 0`; `test` наследует остальные настройки dev, включая
incremental вне CI. При `CI` Cargo по умолчанию отключает incremental, если его
не переопределить. Для этого PR профиль не менялся: репозиторий не создаёт
постоянные внешние fixture targets, а одноразовые сборки ограничены
`TempWorkspace`. Замер до/после приведён в PR.

## Границы владения

- Основной target определяется фактическим выводом `cargo metadata` и
  очищается, только если target и все учитываемые build directories разрешаются
  внутрь текущего workspace, дерево не содержит ошибок/mount boundary, есть
  штатный `CACHEDIR.TAG`, а проверка процессов не обнаружила занятый путь.
  Target, направленный в общий внешний каталог, виден, но автоматически не
  очищается.
- Внешний reusable cache создаётся только командой `cache init`. Маркер
  подтверждает canonical workspace root, cache id и относительный target path.
  Каталог должен принадлежать текущему UID и быть закрыт от группы/остальных.
  `cache run` задаёт Cargo target/build directories и обновляет время
  использования до и после команды. GC рассматривает только такие caches.
- `/tmp` и, если он отличается, `std::env::temp_dir()` обходятся полностью.
  В отчёт входят общий allocated size, top-level count и top-N крупных entries.
  Sparse files учитываются по выделенным блокам, hard links — один раз в каждом
  просмотренном root, symlink не обходятся, другие mount points не пересекаются.
- `TempWorkspace` очищается существующим owner-aware GC. Доказанно живой владелец,
  неизвестный PID/process reference, повреждённый marker или ошибка проверки
  означают `deferred`/`error`, а не попытку удаления.
- Объекты других UID получают `foreign`; объекты без доказанного marker —
  `unknown`. Каталоги, файлы и symlink показываются и никогда не удаляются
  проектом; symlink-цели не обходятся. Ошибки доступа и пропущенные mounts делают
  соответствующий inventory неполным; итоговая сумма тогда является только
  видимой нижней оценкой. Ошибка перечисления external cache прекращает запуск,
  чтобы GC не использовал неполный список.
- `systemd-tmpfiles-clean.timer` показывается диагностически. Глобальную очистку
  чужих временных данных оставляют системе.

## Политика

Единственный tracked источник defaults — [`policy.toml`](policy.toml):

| Объект | Значение | Действие |
|---|---:|---|
| Основной Cargo target warning | 12 GiB | Только предупреждение. |
| Основной Cargo target hard limit | 20 GiB | Dry-run предлагает полный `cargo clean`; `clean --apply` выполняет его после повторной проверки. |
| Совокупные marker-owned external caches hard limit | 10 GiB | Очищать старейшие разрешённые caches, пока размер не станет не выше 5 GiB либо кандидатов не останется. |
| Минимальный возраст external cache | 7 дней | Более свежий cache пропускается. |
| Orphan `TempWorkspace` | Политика существующего владельца | Схема 1 требует TTL не менее 24 часов; схема 2 использует доказательство process identity и текущие ссылки. |
| Обычный top-N `/tmp` | 20 entries | `--detail` показывает все top-level entries. |

`--policy FILE` переопределяет TOML значения. Для тестовых сценариев доступны
byte/second overrides, например `target_hard_limit_bytes=2`; это позволяет
проверять решения на маленьких файлах, не занимая гигабайты.

## Команды

Соберите бинарник один раз из корня workspace:

```bash
cargo build --locked --release -p repository-maintenance
MAINT=./target/release/anki-repository-maintenance
```

Инвентаризация без записи и стабильный JSON:

```bash
$MAINT scan
$MAINT scan --json
$MAINT scan --detail --json
```

План очистки и явное применение:

```bash
$MAINT clean --json
$MAINT clean --apply --json
```

`clean` без `--apply` — dry-run: отчёт показывает планируемое действие по
измеренному target и не запускает Cargo cleaner. Это сохраняет строгую
беззаписность: `cargo clean --dry-run` сам создаёт `.rustc_info.json` в target.
При применении используется обычный `cargo clean` с точно найденными путями.
Cleanup требует maintenance lock и немедленно перепроверяет размер, inode,
mount/error, ownership marker и процессы. Повторный запуск безопасен и при
отсутствии target показывает 0 освобождённых байт.

Создание и использование reusable внешнего cache:

```bash
$MAINT cache init --id local-check
$MAINT cache run --id local-check -- cargo test --workspace --locked
```

Второй пример обновляет marker и направляет оба Cargo каталога сборки в
зарегистрированный target. `cache run` отклоняет явный `--target-dir`, чтобы
команда не обходила marker lifecycle. Cache становится кандидатом GC только
после превышения совокупного hard limit и истечения семидневного возраста.

Opt-in ежедневный user-level systemd timer:

```bash
$MAINT timer install
$MAINT timer status
$MAINT timer uninstall
```

Installer копирует бинарник в `~/.local/bin`, пишет unit-файлы с `%h` вместо
machine-specific пути и сохраняет текущий workspace root в пользовательском
install config. Таймер запускает `clean --apply` один раз в сутки; это
единственный автоматический apply путь. `timer uninstall` выключает и удаляет
свои units, install config и binary. Если user systemd недоступен, ручные команды
продолжают работать.

Все режимы поддерживают `--workspace-root`; `XDG_CACHE_HOME`, `--policy` и
`--temp-root` позволяют изолировать cache, пороги и project TempWorkspace GC в
тестовом sandbox. `--temp-root` задаёт project GC root и дополнительный
инвентаризируемый каталог; системный `/tmp`/`std::env::temp_dir()` всё равно
просматривается полностью.

## `busy`, `deferred` и ограничения

`busy` означает обнаруженный build/test процесс или живого владельца
`TempWorkspace`. `deferred` означает, что возраст ещё не истёк, процесс/marker
нельзя проверить полностью, target общий/внешний или повторная проверка увидела
изменение. `error` отражает ошибку чтения/очистки. Ни одна из этих причин не
разрешает продолжить удаление.

Процессная проверка максимально использует доступные `/proc` cwd, executable,
process identity, родительскую цепочку и открытые fd у процессов workspace.
Между последней проверкой и запуском `cargo clean` остаётся малое окно для
не сотрудничающего с инструментом процесса Cargo: Cargo не предоставляет lock,
который удерживает безусловную очистку всего target от параллельного нового
build. При обнаружении уже работающего процесса очистка откладывается; timer не
запускает пересборку.

Полный `/tmp` может включать закрытые root/systemd каталоги. Они отражаются как
foreign/error; их содержимое может быть недоступно, поэтому JSON помечает
inventory как `complete: false`. Проект не подменяет системный
`systemd-tmpfiles` и не удаляет неизвестные evidence ZIP, даже если их имена
содержат `anki-decks`.
