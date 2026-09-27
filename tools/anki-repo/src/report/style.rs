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
//! Базовые правила намеренно скромные и не выдаются за поведение Anki: ночной
//! режим, масштаб шрифта интерфейса и темы устройства здесь не воспроизводятся.
//! Точное поведение оформления задаёт модель, а не этот файл.

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
}

* { box-sizing: border-box; }

body.report {
  margin: 0;
  padding: 0 0 4rem 0;
  background: var(--report-bg);
  color: var(--report-ink);
  font: 15px/1.5 -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
}

.report-header {
  background: var(--report-panel);
  border-bottom: 1px solid var(--report-line);
  padding: 1.5rem 2rem;
}

.report-title { margin: 0 0 .25rem 0; font-size: 1.4rem; }
.report-subtitle { margin: 0; color: var(--report-muted); font-size: .9rem; }
.report-subtitle code { background: var(--report-bg); padding: .1rem .3rem; border-radius: 3px; }

.report-main { padding: 1.5rem 2rem; max-width: 1200px; }

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

.report-preview { width: 100%; border: 1px solid var(--report-line); border-radius: 6px; background: #fff; }
.report-preview-host { margin: .5rem 0 0 0; }
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
body {
  background: #ffffff;
  color: #000000;
}
.card {
  font-family: arial;
  font-size: 20px;
  text-align: center;
  color: #000000;
  background-color: #ffffff;
  padding: 1em;
  margin: 0 auto;
}
hr#answer { border: 0; border-top: 1px solid #c8c8c8; margin: .6em 0; }
img { max-width: 100%; }
"#;
