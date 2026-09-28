//! Статические стили визуального отчёта.
//!
//! Два независимых набора:
//!
//! - [`REPORT_CSS`] — оформление самого отчёта. Все селекторы начинаются с
//!   `report-`, чтобы стиль инструмента не мог случайно изменить внешний вид
//!   карточки: карточку оформляет CSS модели, а не отчёт.
//! - [`CARD_BASE_CSS`] — приближение базовых стилей Anki, которые приложение
//!   подкладывает под CSS модели. Модель идёт после базовых правил, поэтому
//!   перекрывает их, как и в Anki.
//!
//! Базовые правила намеренно скромные и не выдаются за поведение Anki: масштаб
//! шрифта интерфейса и темы устройства здесь не воспроизводятся. Ночной режим
//! воспроизводится ровно настолько, насколько это нужно для визуального ревью
//! CSS модели: тот же класс `nightMode`, который ставит Anki, и разумные тёмные
//! значения по умолчанию для моделей, которые их не задают сами.
//!
//! Цвета карточки вынесены в переменные не ради темы отчёта, а чтобы ночной
//! класс мог их переопределить, не поднимая специфичность `.card`: иначе базовые
//! правила отчёта перебивали бы CSS модели, и модель не смогла бы задать фон
//! карточки ни в светлом режиме, ни в ночном.

/// CSS отчёта.
pub const REPORT_CSS: &str = r#":root {
  color-scheme: light;
  --report-ink: #1c1f23;
  --report-muted: #5c6470;
  --report-line: #d7dbe0;
  --report-bg: #f6f7f9;
  --report-panel: #ffffff;
  --report-accent: #2f6feb;
  --report-created: #1f7a45;
  --report-changed: #9a6100;
  --report-retired: #7a3fa0;
  --report-removed: #a32323;
  --report-ins: #d8f3dc;
  --report-del: #ffdcdc;
  /* Читаемая длина строки для прозы отчёта. Это не предел ширины отчёта:
     визуальное сравнение «до»/«после» живёт в собственной, более широкой
     раскладке и меряется не символами, а кадром карточки. Значение подобрано по
     самому мелкому тексту отчёта (`.85rem` у ограничений и диагностики): при нём
     строка остаётся в пределах восьмидесяти символов, а основной текст — около
     семидесяти. */
  --report-reading-width: 68ch;
  --report-gutter: clamp(1rem, 3vw, 3rem);
}

* { box-sizing: border-box; }

body.report {
  margin: 0;
  padding: 0 0 4rem 0;
  background: var(--report-bg);
  color: var(--report-ink);
  font: 15px/1.5 -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
}

/* Ночная тема оболочки. Она переключает только переменные отчёта: CSS модели в
   превью живёт в отдельном документе и themed-классом самого отчёта не
   затрагивается. */
body.report.report-night {
  color-scheme: dark;
  --report-ink: #e6e8eb;
  --report-muted: #a7aeb8;
  --report-line: #3a4048;
  --report-bg: #191c20;
  --report-panel: #23272c;
  --report-accent: #7aa7ff;
  --report-created: #5fcf92;
  --report-changed: #e3b25a;
  --report-retired: #c79ae8;
  --report-removed: #f08a8a;
  --report-ins: #1d3a26;
  --report-del: #3d2222;
}

.report-header {
  background: var(--report-panel);
  border-bottom: 1px solid var(--report-line);
  padding: 1.5rem var(--report-gutter) 1.25rem var(--report-gutter);
}

.report-title { margin: 0 0 .25rem 0; font-size: 1.4rem; }
.report-subtitle { margin: 0; color: var(--report-muted); font-size: .9rem; }
.report-subtitle code { background: var(--report-bg); padding: .1rem .3rem; border-radius: 3px; }

/* Пояснение к отчёту — проза, поэтому оно ограничено читаемой длиной строки, а
   не растянуто на всю ширину окна вместе с превью карточек. */
.report-header .report-hint { max-width: var(--report-reading-width); }

/* Панель управления живёт отдельно от шапки: на длинном отчёте переключатель
   темы нужен у той карточки, которую сейчас смотрят, а не только наверху.
   `sticky` оставляет его в потоке — он не отрывается от раскладки и занимает
   собственную полосу высотой в одну строку, а не плавающий блок поверх текста. */
.report-toolbar {
  position: sticky;
  top: 0;
  z-index: 10;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: .4rem 1rem;
  padding: .45rem var(--report-gutter);
  background: var(--report-panel);
  border-bottom: 1px solid var(--report-line);
  box-shadow: 0 1px 2px rgb(0 0 0 / 5%);
}

.report-toolbar-label { color: var(--report-muted); font-size: .85rem; }

/* Проза отчёта: ограничения, диагностика, неподдержанные конструкции. Предел
   ширины стоит только на текстовых секциях — карточки и их сравнение остаются
   во всю доступную ширину. */
.report-section-prose { max-width: var(--report-reading-width); }

/* Справочный блок ограничений свёрнут по умолчанию и раскрывается нативным
   `details`, то есть без участия runtime: читатель, у которого JavaScript
   выключен, всё равно увидит, что отчёт чего-то не покрывает. */
.report-details > summary {
  cursor: pointer;
  list-style: none;
  display: flex;
  align-items: baseline;
  gap: .5rem;
}

.report-details > summary::-webkit-details-marker { display: none; }

.report-details > summary::after {
  content: "";
  margin-left: auto;
  width: .45rem;
  height: .45rem;
  border-right: 2px solid var(--report-muted);
  border-bottom: 2px solid var(--report-muted);
  transform: rotate(45deg) translate(-.1rem, -.1rem);
  transition: transform .15s ease;
}

.report-details[open] > summary::after { transform: rotate(-135deg) translate(-.15rem, .1rem); }
.report-details[open] > summary { margin-bottom: .6rem; }

.report-details > summary h2 { margin: 0; font-size: 1.1rem; }
.report-details > summary:focus-visible { outline: 2px solid var(--report-accent); outline-offset: 2px; }

.report-summary-count {
  color: var(--report-muted);
  font-size: .85rem;
  font-variant-numeric: tabular-nums;
}

/* Отчёт использует доступную ширину: превью карточки — это и есть то, что
   сравнивает ревьюер, и искусственный предел ширины заставлял бы смотреть
   карточку в узкой колонке рядом с пустым полем. Прозу ограничивает не эта
   ширина, а `.report-section-prose`. */
.report-main { padding: 1.5rem var(--report-gutter); max-width: none; }

.report-theme {
  display: inline-flex;
  gap: .25rem;
  margin: 0 0 0 auto;
  padding: .15rem;
  border: 1px solid var(--report-line);
  border-radius: 999px;
  background: var(--report-bg);
}

.report-theme-option {
  font: inherit;
  font-size: .85rem;
  padding: .2rem .8rem;
  border: 0;
  border-radius: 999px;
  background: transparent;
  color: var(--report-muted);
  cursor: pointer;
}

.report-theme-option[aria-pressed="true"] {
  background: var(--report-panel);
  color: var(--report-accent);
  font-weight: 600;
  box-shadow: 0 1px 2px rgb(0 0 0 / 12%);
}

.report-counts {
  display: flex;
  flex-wrap: wrap;
  gap: .5rem;
  margin: 1rem 0 0 0;
  list-style: none;
  padding: 0;
}

.report-count {
  border: 1px solid var(--report-line);
  border-radius: 999px;
  padding: .2rem .7rem;
  background: var(--report-panel);
  font-size: .85rem;
}

.report-count-created { color: var(--report-created); border-color: currentColor; }
.report-count-changed { color: var(--report-changed); border-color: currentColor; }
.report-count-retired { color: var(--report-retired); border-color: currentColor; }
.report-count-removed { color: var(--report-removed); border-color: currentColor; }

.report-section {
  background: var(--report-panel);
  border: 1px solid var(--report-line);
  border-radius: 8px;
  margin: 0 0 1.25rem 0;
  padding: 1rem 1.25rem;
}

.report-section > h2 { margin: 0 0 .25rem 0; font-size: 1.1rem; }
.report-section > .report-hint { margin: 0 0 .75rem 0; color: var(--report-muted); font-size: .85rem; }

.report-card {
  border-top: 1px solid var(--report-line);
  padding: .85rem 0 1rem 0;
}

.report-card:first-of-type { border-top: 0; }
.report-card > h3 { margin: 0 0 .35rem 0; font-size: 1rem; word-break: break-word; }

.report-meta {
  margin: 0 0 .6rem 0;
  color: var(--report-muted);
  font-size: .8rem;
  display: flex;
  flex-wrap: wrap;
  gap: .15rem .9rem;
}

.report-meta code { word-break: break-all; }

.report-tags { margin: .35rem 0; font-size: .85rem; }
.report-tag { display: inline-block; border: 1px solid var(--report-line); border-radius: 4px; padding: .05rem .35rem; margin: 0 .25rem .25rem 0; }
.report-tag-added { border-color: var(--report-retired); color: var(--report-retired); }
.report-tag-removed { border-color: var(--report-removed); color: var(--report-removed); text-decoration: line-through; }

.report-fields { border-collapse: collapse; width: 100%; font-size: .85rem; margin: .5rem 0; }
.report-fields th, .report-fields td { border: 1px solid var(--report-line); padding: .3rem .45rem; text-align: left; vertical-align: top; }
.report-fields th { width: 12rem; background: var(--report-bg); font-weight: 600; }
.report-fields td { word-break: break-word; }

.report-diff { margin: .5rem 0; font-size: .85rem; }
.report-diff th, .report-diff td { border: 1px solid var(--report-line); padding: .3rem .45rem; vertical-align: top; }
.report-diff th { background: var(--report-bg); text-align: left; }
.report-diff td { word-break: break-word; }
.report-token-ins { background: var(--report-ins); }
.report-token-del { background: var(--report-del); text-decoration: line-through; }

/* Два состояния одной заметки сравниваются бок о бок на широком экране и
   складываются вертикально на узком. Колонки заданы `minmax(0, 1fr)`, чтобы
   широкое превью не растягивало сетку и не ломало страницу. */
.report-compare {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: .9rem;
  margin: .6rem 0 0 0;
}

@media (min-width: 1000px) {
  .report-compare { grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); align-items: start; }
}

.report-compare-side { min-width: 0; }
.report-compare-label { margin: 0 0 .25rem 0; font-size: .8rem; font-weight: 600; }
.report-compare-side[data-state="before"] .report-compare-label { color: var(--report-changed); }
.report-compare-side[data-state="after"] .report-compare-label { color: var(--report-created); }

.report-preview-frame {
  border: 1px solid var(--report-line);
  border-radius: 6px;
  background: #fff;
  overflow: hidden;
}

/* Рамку кадра несёт обёртка, а не сам `<iframe>`: высота кадра измеряется по
   содержимому карточки и ставится на внешний box. Пока рамка была на кадре,
   глобальный `box-sizing: border-box` вычитал её пиксели из измеренной высоты,
   и содержимому не хватало ровно её — браузер показывал внутри превью
   собственный scrollbar. Без рамки и отступов у кадра `style.height` совпадает
   с высотой его содержимого, и запаса против дробного округления хватает. */
.report-preview {
  display: block;
  width: 100%;
  border: 0;
  /* Кадр растёт по измеренной высоте содержимого, поэтому собственные полосы
     прокрутки ему не нужны: прокручивается сама страница отчёта. */
  overflow: hidden;
  background: #fff;
}
.report-preview-host { margin: .5rem 0 0 0; min-width: 0; }
.report-preview-label { margin: 0 0 .25rem 0; font-size: .8rem; color: var(--report-muted); }
.report-preview-link { font-size: .8rem; }

.report-issues { margin: .35rem 0 0 0; padding-left: 1.2rem; font-size: .85rem; color: var(--report-removed); }
.report-issues li { margin: .1rem 0; }

.report-diagnostics { font-size: .85rem; }
.report-diagnostics code { font-weight: 600; }
.report-diagnostic { border-top: 1px dashed var(--report-line); padding: .4rem 0; }
.report-diagnostic:first-of-type { border-top: 0; }
.report-severity-warning { color: var(--report-removed); }
.report-severity-info { color: var(--report-muted); }

.report-limits { font-size: .85rem; }
.report-limits li { margin: .2rem 0; }
"#;

/// Приближение базовых стилей Anki под CSS модели.
pub const CARD_BASE_CSS: &str = r#"html, body { margin: 0; padding: 0; }
:root {
  --report-card-bg: #ffffff;
  --report-card-fg: #000000;
}
body {
  background: var(--report-card-bg);
  color: var(--report-card-fg);
}
.card {
  font-family: arial;
  font-size: 20px;
  text-align: center;
  color: var(--report-card-fg);
  background-color: var(--report-card-bg);
  padding: 1em;
  margin: 0 auto;
}
hr#answer { border: 0; border-top: 1px solid #c8c8c8; margin: .6em 0; }
img { max-width: 100%; }

/* Ночной режим: класс ставит runtime отчёта, как это делает Anki. Значения —
   разумное приближение тёмной базы Anki для моделей, которые не задают ночные
   правила сами; модель по-прежнему идёт после этих правил и перекрывает их. */
.nightMode {
  --report-card-bg: #2f2f31;
  --report-card-fg: #e6e6e6;
}
.nightMode hr#answer { border-top-color: #55595f; }
.nightMode a { color: #7aa7ff; }
.nightMode img { opacity: .92; }

/* Anki показывает звук кнопкой повтора. Класс `replay-button` сохранён как
   стилевой хук модели, а сам проигрыватель — обычный локальный `audio`, потому
   что отчёт обязан давать рабочее воспроизведение без сети и без Anki. */
.replay-button { display: inline-block; vertical-align: middle; }
.report-audio { display: inline-block; vertical-align: middle; max-width: 100%; height: 2em; }
.report-audio-missing {
  border-bottom: 1px dotted #a32323;
  color: #a32323;
}
.nightMode .report-audio-missing { color: #f08a8a; border-bottom-color: #f08a8a; }

/* Плейсхолдер отсутствующей картинки стоит ровно на месте media. Браузерная
   иконка сломанного файла молчит о том, чего не хватает, и сама по себе
   участвует в inline-раскладке; этот элемент говорит basename вслух и остаётся
   предсказуемым по размеру. Размеры в `em`, чтобы плейсхолдер следовал
   масштабу шрифта карточки, а не отчёта. */
.report-media-missing {
  display: inline-block;
  max-width: 100%;
  padding: .15em .5em;
  border: 1px dashed currentColor;
  border-radius: .25em;
  font-size: .7em;
  line-height: 1.4;
  color: #a32323;
  background-color: rgb(163 35 35 / 8%);
  vertical-align: middle;
  word-break: break-all;
}
.nightMode .report-media-missing { color: #f08a8a; background-color: rgb(240 138 138 / 12%); }
"#;
