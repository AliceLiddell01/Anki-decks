---
name: anki-card-create
description: >-
  Создание новых карточек в существующей CrowdAnki-колоде от пользовательского
  запроса до проверки результата, включая configured processors, kanji assets и
  продолжение ожидающего ручной проверки batch. Применяй к просьбам создать
  карточку или batch карточек и продолжить такой flow. Не применяй к правке
  существующих заметок, generic Git, CodeRabbit или объяснению Anki.
---

# Создание карточек CrowdAnki

## Область

Этот skill владеет агентской оркестрацией создания **новых** заметок в
существующей колоде и модели. `tools/anki-repo/README.md` владеет контрактом
`models`, `create`, `validate`, `visual-report` и processor policy;
`tools/asset-store/README.md` — контрактом `kanji-assets`, asset integrity,
effective trust и batch state. Общий Git/GitHub lifecycle выполняй через
`anki-git-workflow`; здесь действуют дополнительные ограничения для публикации
asset corpus. Не создавай wrapper, вторую копию manifest parser или собственную
CV/acquisition логику.

При первом обращении к колоде прочитай `.codex/context/INDEX.md`, затем
профильных владельцев. Не загружай многомегабайтный `deck.json` целиком.

## Создание и частичный прогресс

1. Определи целевой экспорт и узел колоды по запросу и фактическим данным.
   Вызови `anki-repo models` с точным selector; проверь UUID модели, имена и
   порядок `flds[].ord`, примеры значений и шаблоны. Не выводи содержание поля
   только из его имени. Если цель или содержательное значение неоднозначны,
   уточни именно недостающее.
2. Прочитай tracked `.anki-repo/create.yaml`. Сопоставь фактический
   `note_models[].crowdanki_uuid` и точные имена полей с policy. Выполняй
   `processors` в объявленном порядке. Отсутствие правила не разрешает media.
   Не угадывай media extension или имя canonical файла. Для `kanji_assets`
   извлекай поддерживаемые kanji identities только из настроенного поля,
   исключая kana и прочие символы; дедуплицируй зависимости между notes.
3. Подготовь processor dependencies через их владельцев. Для `kanji_assets`
   используй batch/review контракт `kanji-assets` из
   [references/kanji-batch.md](references/kanji-batch.md). Доверенными считай
   только assets, которые asset owner подтверждает как effective verified для
   текущих bytes. Составь media references по его canonical результату.
4. Раздели notes по состоянию **всех** dependencies. Готовые notes запускай
   через `anki-repo --json create ... --request ...` в dry-run; изучи полный
   machine-readable plan, blockers и per-note outcomes. `--apply` выполняй
   только для notes с разрешёнными зависимостями и успешным dry-run. Остальные
   оставь pending с причиной, не добавляя missing media. После каждого раунда
   asset resolution можно продвигать вновь готовые notes, не ожидая ручного
   решения по независимым notes.
5. Сохраняй resolved create request / GUID и статус notes в локальном рабочем
   состоянии вне `decks/**` и Git. При возобновлении сверяй его с фактическим
   экспортом и `create` outcomes, затем создавай только новые разблокированные
   notes. Не считай память чата источником возобновления. Повтор завершённого
   flow должен сходиться к `already_applied`/no-op, а не создавать дубликаты.
6. После каждой записи проверь фактический JSON через `anki-repo validate` и
   сравни состояние до/после через `visual-report` в каталоге вне `decks/**`.
   `visual-report` требует явные `--before` и `--after`, поэтому до первого
   `--apply` сними состояние «до» в отдельный каталог вне `decks/**` (копия
   export или `git worktree` закоммиченного состояния); иначе сравнивать будет
   не с чем. Осмотри карточки и media в отчёте; статическое превью не заменяет
   Anki.

Показывай пользователю раздельные числа/списки: created; ready, но только
dry-run; blocked by unresolved assets; blocked по другим причинам; auto
verified, human verified, awaiting human review и scheduled for reacquire
assets. Независимый item failure не отменяет успешные notes/assets.

## Решения человека

Передавай `kanji-assets` только структурированное действие для **текущего**
`AssetIdentity + SHA-256`. Ясное «漢 - подтверждён» означает semantic approval
exact candidate. Ясное «字 - артефакт битый» или «не тот иероглиф» означает
reject exact candidate и targeted reacquire этой identity. Пользовательское
semantic решение имеет приоритет над automated semantic status, включая
`REJECTED`, но не обходит format/decode/integrity/path boundary. Не называй
human decision криптографической подписью. Неоднозначный текст не меняет state:
уточни действие или identity. Если hash сменился, запроси новое решение.

Перед первым `batch start` зафиксируй во внешнем workflow state исходную точку
asset batch: push remote, default branch и точный SHA этой base. Проверь
`kanji-corpus-gate`, затем сравни canonical scope `.asset-store/kanji` с этим
SHA через `git diff` и `git status --short --untracked-files=all`; до batch там
не должно быть отличий. Сохрани результат `kanji-assets list --output json` как
owner-снимок исходных verified identities, не читая manifest самостоятельно.
Если store отсутствует и в baseline нет canonical corpus, исходный owner-снимок
пуст: `list` завершится ненулевым exit code с `outcome = blocked` и точным
`error.code = "store_missing"`. Пустым состоянием считай только этот код и
только после проверки отсутствия store directory; любая другая ошибка `list`
закрывает publication.
Если remote base или чистое исходное состояние установить нельзя, карточечный
flow и batch могут продолжаться, но asset publication остаётся
`LOCAL_READY_TO_PUBLISH` до восстановления точной baseline.

## Публикация canonical assets

Только после завершения automated rounds и всех требуемых human decisions
выдели фактический canonical corpus delta. Сравни текущий результат
`kanji-assets list --output json` с сохранённым owner-снимком: изменившиеся
identity должны принадлежать requested set этого batch, а их итоговый exact SHA
и effective trust должны совпадать с `batch status`. Неизменившиеся identity
сохраняют baseline. Любая посторонняя delta закрывает публикацию: сохрани state
как `LOCAL_READY_TO_PUBLISH`, не включай и не удаляй её и не запускай acquisition
заново. При нулевом delta
branch/commit не создавай.

Иначе создай поздно отдельную узнаваемую ветку с устойчивым для resume и
collision-resistant именем `kanji-assets/<batch-id>`. Основание ветки — только
сохранённый SHA исходной remote base, а не текущий feature HEAD. Изолируй её в
отдельном worktree, не переключая текущий checkout и не перенося его чужие
изменения. Перенеси из source store canonical corpus bytes/metadata, исключив
`.runtime`, `.tmp`, lock и прочие runtime/browser/review artifacts. Так как
начальный canonical corpus был сверен с baseline, source сохраняет прежние
identity и добавляет только delta завершённого batch. Сверь owner-снимок и
canonical files source/worktree; затем stage явным allowlist фактически
существующих принадлежащих corpus путей: `.owner.json`, `manifest.json`,
`assets/*.gif`, `assets/*.png` и только metadata, которая действительно нужна
store. Проверь staged diff и `git status`: там не должно быть decks,
runtime/review/browser artifacts, acceptance data, исходного кода или чужих
файлов. Создай локальный asset-only commit даже без GitHub auth.

`kanji-corpus-gate` запускай кодом **текущего** checkout против worktree как
CWD: worktree стоит на baseline SHA, и его собственный бинарник читает только
manifest schema, существовавший в baseline. Точная команда и поведение при
отвергнутом schema — в
[references/kanji-batch.md](references/kanji-batch.md).

Этот asset-only commit — **вторая** ветка, а не замена ветки текущего work item,
и общий Git lifecycle по-прежнему принадлежит `anki-git-workflow`. Публикуй её
только при наличии отдельного явного основания в текущем запросе пользователя
на вторую asset-only ветку; без такого основания остановись на локальном commit
и состоянии `LOCAL_READY_TO_PUBLISH`. Не переноси в неё исходный код, skill или
decks и не делай её веткой текущего PR.

Перед remote push через `anki-git-workflow` fetch фактическую актуальную base и
объедини её с ещё не опубликованной asset branch по правилам разрешения
конфликтов этого skill. Не подставляй целиком source `manifest.json` поверх
актуальной base и не выбирай `ours`/`theirs` для corpus вслепую. При конфликте,
который нельзя разрешить с сохранением обеих сторон, сохрани локальный commit и
batch state, запрети push и сообщи `LOCAL_READY_TO_PUBLISH`. После успешного
merge повторно запусти `kanji-corpus-gate`, проверь сохранность независимых
base identities и убедись, что diff относительно fetched base содержит только
canonical asset corpus и изменения identities этого batch. Затем выполни
описанный ниже admin gate. Отсутствие доказанного admin переводит публикацию в
`LOCAL_READY_TO_PUBLISH` и не запускает acquisition заново.

Перед **каждым** remote push докажи для host и owner/repository фактического
remote: активную GitHub CLI session, её login и рассчитанный repository
`permission = admin` через GitHub API. Установи host, owner и repository из
фактического push remote. Gate связывает credential, которым пойдёт сам push,
поэтому он открывается только когда push remote — HTTPS и его credential
helper принадлежит той же GitHub CLI session (например
`credential.https://<host>.helper` = `!gh auth git-credential`, как после
`gh auth setup-git`). SSH remote, иной helper или helper, который нельзя
подтвердить, закрывают gate: проверенный login иначе не относится к тому, чем
аутентифицируется `git push`. Если в окружении заданы `GH_TOKEN`,
`GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN` или `GITHUB_ENTERPRISE_TOKEN`, считай
active CLI account неоднозначной и закрой gate. Иначе проверь exit status
`gh auth status --active --hostname "$host"`, получи active login без показа
ответа пользователю:

```bash
login="$(gh api --hostname "$host" user --jq '.login')"
permission="$(gh api --hostname "$host" \
  "repos/$owner/$repo/collaborators/$login/permission" --jq '.permission')"
```

Доступ разрешает push только при точном ответе `permission == admin` для этой
identity и repository; `write`, `maintain`, неизвестный ответ, ошибка API или
auth запрещают push. Не используй `gh auth token`, не показывай credentials и
не выводи право из username, owner или возможности push. Поведение команд
сверено с [официальной документацией `gh auth status`](https://cli.github.com/manual/gh_auth_status),
[`gh api`](https://cli.github.com/manual/gh_api) и [GitHub endpoint permissions](https://docs.github.com/en/rest/collaborators/collaborators#get-repository-permissions-for-a-user).
После успешного gate используй `anki-git-workflow` для явного refspec и проверки
exact remote SHA. При возобновлении publication повтори gate, не повторяя asset
batch. Merge не выполняй без отдельной текущей команды пользователя.

| Проверка | Состояние публикации |
|---|---|
| Нет активной/валидной auth, нельзя получить active login, заданы token env vars | `LOCAL_READY_TO_PUBLISH`; push закрыт |
| Push remote не HTTPS или его credential helper не подтверждён как та же GitHub CLI session | `LOCAL_READY_TO_PUBLISH`; push закрыт |
| Permission `read`, `write` (включая `maintain`) или `none` | `LOCAL_READY_TO_PUBLISH`; push закрыт |
| Endpoint/API ошибка, неверная identity/repository или неизвестное значение | `LOCAL_READY_TO_PUBLISH`; push закрыт |
| Нет отдельного явного основания в текущем запросе на вторую asset-only ветку | Локальный asset commit; push закрыт |
| Тот же active login получил точный `permission = admin` для push remote repo | gate открыт; продолжить через `anki-git-workflow` |

Точные команды и контрольные точки возобновления описаны в
[references/kanji-batch.md](references/kanji-batch.md). Перед исполнением сверь
versioned CLI interface по актуальным `--help` и owner README: domain owner
может изменить синтаксис, но не смысловые инварианты этого workflow.
