---
name: anki-git-workflow
description: >-
  Единый владелец Git/GitHub lifecycle текущего checkout: task branch, staging,
  commit, push, exact remote-SHA verification, PR create/edit, Draft/Ready,
  разрешённый merge, конфликты с base, cleanup. Применяй автоматически, когда
  обычная публикуемая задача доходит до Git/GitHub: локальный commit её не
  завершает — цепочка идёт до проверенного удалённого состояния и Draft PR.
  Сюда же «закоммить», «запушь», «создай PR», «слей PR». Не для объяснения Git
  без mutations и не для CodeRabbit review logic.
whenToUse: >-
  Текущая работа достигает Git/GitHub lifecycle: создаётся или продолжается
  task branch, изменение нужно застейджить, закоммитить и опубликовать, PR
  требуется создать или обновить, перевести в Ready, разрешить конфликт с base,
  выполнить отдельно разрешённый merge либо cleanup после него.
---

# Anki-decks: Git/GitHub lifecycle обычной задачи

## Назначение

Skill — **единственный владелец** общей процедуры Git/GitHub для текущего
checkout: рабочая ветка, index, commit, push, проверка удалённого состояния,
создание и обновление PR, Draft/Ready, отдельно разрешённый merge и post-merge
cleanup.

Skill не владеет:

- содержанием работы: что именно нужно изменить, определяет сама задача и
  профильные владельцы (`AGENTS.md`, `.codex/context/`, `tools/anki-repo`);
- repository verification matrix — её владельцы перечислены в `AGENTS.md` и
  `.codex/context/`, в первую очередь `.codex/context/03-VERIFICATION.md`;
- CodeRabbit-specific логикой: provider invocation, findings, triage, iteration
  semantics, rate limit и clean-pass semantics принадлежат
  `anki-coderabbit-review`;
- generic code review, содержательным суждением о карточках и правкой колод;
- чужими PR и чужими ветками: skill обслуживает текущий work item.

Соподчинённость владельцев:

```text
anki-git-workflow
    └─ общая процедура Git/GitHub: branch / index / commit / push /
       exact remote verification / PR create+edit / Draft+Ready /
       разрешённый merge / conflicts / cleanup

anki-coderabbit-review
    └─ только CodeRabbit workflow: provider invocation / completion /
       findings / triage / iteration semantics / rate limit /
       clean-pass semantics
       и делегирует общую публикацию anki-git-workflow
```

Не создавай второго владельца этой процедуры в `AGENTS.md`, `.codex/context/`
или в CodeRabbit skill, и не переноси её в них. Skill не создаёт собственную
обёртку над `git`/`gh`, своего CLI, state machine, persistent publication journal
или отдельное хранилище состояния: для этого репозитория достаточно дисциплины
и нативных инструментов.

## Trigger contract

Skill **не требует** буквального имени skill или специальной фразы. Он
применяется, когда работа достигает Git/GitHub lifecycle:

- создание или продолжение task branch;
- staging;
- commit;
- push;
- создание или обновление PR;
- Draft/Ready;
- проверка удалённого состояния, CI и reviews перед передачей пользователю;
- разрешение конфликтов с base;
- отдельно разрешённый merge;
- post-merge cleanup;
- продолжение уже начатой публикации.

Явные запросы пользователя — «закоммить», «запушь», «создай PR», «обнови PR»,
«слей PR», «сделай Draft PR» — маршрутизируются сюда же.

Skill **не активируется** для:

- простого объяснения Git/GitHub без mutations;
- generic code review;
- CodeRabbit-specific review logic — это `anki-coderabbit-review`;
- чтения истории, веток или PR только ради исследования, если Git lifecycle не
  выполняется;
- задачи по изменению самого этого skill как повода выполнить дополнительные
  GitHub actions сверх текущего work item.

## Terminal-state contract: commit не равен публикации

Это главный контракт skill.

Если обычная repository task предполагает публикацию результата и пользователь
явно не ограничил scope локальной работой, состояние

```text
есть новый локальный commit
remote branch его не содержит
```

**не является** завершением задачи. После commit workflow обязан без
дополнительного сообщения пользователя «теперь push» продолжить публикацию и
довести её до подтверждённого удалённого состояния.

Ожидаемая цепочка обычной публикуемой задачи:

```text
изменение
→ применимая verification
→ проверенный staging
→ commit
→ push текущей task branch
→ проверка exact remote HEAD
→ создание или актуализация Draft PR
→ повторное чтение и проверка опубликованного PR
→ передача пользователю на review
```

Merge в этот автоматический lifecycle **не входит**: он выполняется только по
отдельной текущей явной команде пользователя (см.
[`references/pr-lifecycle.md`](references/pr-lifecycle.md), § «Merge»).

Допустимый override — только явное указание пользователя в текущем запросе:
«только локально», «только commit», «не push». Такое указание ограничивает
цепочку ровно настолько, насколько сказано, и относится к текущему work item.

Локальный `exit code` push сам по себе не является доказательством
postcondition, и устаревший remote-tracking ref — тоже. Публикация подтверждена
только точным сравнением ожидаемого локального HEAD с фактическим SHA
соответствующей удалённой ветки, прочитанным из remote (см.
[`references/publication.md`](references/publication.md)).

## Native tooling

Граница инструментов:

- штатный `git` — локальная история, `status`/`diff`, branch, staging, commit,
  fetch, merge/rebase по разрешённой политике, push и чтение удалённой ссылки;
- `gh` — GitHub mutations текущего work item: PR create/edit, Draft/Ready и
  разрешённый merge;
- GitHub MCP/connector — предпочтительное структурированное чтение удалённого
  GitHub state, когда он доступен и реально удобнее;
- `gh ... --json` — допустимый fallback и короткая проверка состояния.

Версионно-зависимые операции не описываются как вечный контракт: конкретную
версию `gh`, имена полей и flags проверяй по фактическому interface установленного
инструмента там, где это действительно нужно, а не по памяти и не по тексту этого
skill.

## Инварианты

- Одна самостоятельная задача — одна рабочая ветка и один PR.
- Не работай напрямую в default branch. Ветки не переключаются молча.
- Не считай локальный commit доказательством публикации.
- Не считай exit code push доказательством удалённого postcondition без чтения
  фактической удалённой ссылки.
- Разрешение на commit и push не разрешает merge.
- Не создавай дублирующий PR для того же repository/base/head.
- Не хардкодь номер PR, имя ветки, SHA, номер stage, абсолютные локальные пути
  и имя пользователя: ни в этом skill, ни в его references.
- Не подмешивай и не уничтожай неизвестные пользовательские изменения.
- Merge, force-push, переписывание опубликованной истории и удаление неизвестных
  удалённых ветвей требуют отдельного явного основания в текущем запросе.
- Не ослабляй применимую repository verification и не выдумывай фиктивные
  проверки ради зелёного результата.
- Не расширяй scope skill инфраструктурой «на будущее».

## Подробный workflow

- [`references/publication.md`](references/publication.md) — branch, staging,
  commit, push, exact remote verification, протокол неизвестного результата
  сетевой операции и запрещённые операции публикации.
- [`references/pr-lifecycle.md`](references/pr-lifecycle.md) — PR identity,
  repository-local source файла body, структура body, create/edit и повторная
  проверка опубликованного PR, Ready, merge, конфликты с base и cleanup.

Читай нужный reference перед выполнением соответствующего шага: `SKILL.md` задаёт
контракт и границы, references владеют порядком действий.
