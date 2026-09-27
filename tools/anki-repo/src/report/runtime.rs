//! Собственный runtime визуального отчёта.
//!
//! Отчёт обязан открываться офлайн как обычный файл и при этом оставаться
//! интерактивным: превью должно получать фактическую высоту содержимого, а
//! переключение светлой и ночной темы — доходить до каждой уже открытой карточки.
//! Ни HTML, ни CSS этого не дают: `<iframe>` не умеет измерять содержимое, а
//! доступ к документу превью из родителя запрещён, потому что `file://`-документы
//! считаются разными origin'ами.
//!
//! Граница доверия поэтому описана точно, а не лозунгом «в отчёте нет
//! JavaScript»:
//!
//! - **Разрешено**: этот runtime. Он детерминированно сгенерирован самим
//!   `visual-report`, лежит прямо в записанных файлах и говорит только по
//!   протоколу [`HELLO_MESSAGE`] / [`HEIGHT_MESSAGE`] / [`THEME_MESSAGE`].
//! - **Запрещено**: любой внешний код и любой сетевой доступ. Ни CDN, ни
//!   `import`, ни `fetch`: отчёт не ходит в сеть ни при генерации, ни при
//!   открытии.
//! - **Запрещено**: `<script>` из шаблона Anki или из значения поля. Такой
//!   `<script>` остаётся неподдержанной конструкцией: сторона считается
//!   неотрисованной и отмечается как неполное превью. Runtime отчёта никогда не
//!   подхватывает и не исполняет шаблонный код.
//!
//! Обмен идёт через `postMessage`, потому что это единственный способ, который
//! работает между `file://`-документами без сервера. Родитель отвечает на
//! [`HELLO_MESSAGE`] текущей темой, поэтому поздно загруженный кадр не остаётся
//! светлым: гонки «до в тёмной, после в светлой» не возникает.

/// Класс, которым Anki помечает ночной режим.
///
/// Селекторы модели `.card.nightMode` и `.nightMode .…` обязаны совпадать,
/// поэтому класс ставится и на корневые элементы документа, и на саму карточку.
pub const NIGHT_CLASS: &str = "nightMode";

/// Значение светлой темы.
pub const THEME_LIGHT: &str = "light";

/// Значение ночной темы.
pub const THEME_NIGHT: &str = "night";

/// Атрибут корня страницы отчёта с выбранной темой.
pub const THEME_ATTRIBUTE: &str = "data-report-theme";

/// Атрибут кнопок переключения темы.
pub const THEME_CONTROL_ATTRIBUTE: &str = "data-report-theme-value";

/// Класс тела страницы отчёта в ночной теме.
pub const REPORT_NIGHT_CLASS: &str = "report-night";

/// Класс кадра превью.
pub const PREVIEW_CLASS: &str = "report-preview";

/// Сообщение кадра: «я загрузился, вот моя высота».
pub const HELLO_MESSAGE: &str = "report:hello";

/// Сообщение кадра: «моя высота изменилась».
pub const HEIGHT_MESSAGE: &str = "report:height";

/// Сообщение родителя: «примени тему».
pub const THEME_MESSAGE: &str = "report:theme";

/// Высота кадра, пока runtime её не измерил.
///
/// Это именно запасное значение для случая, когда JavaScript недоступен, а не
/// рабочий размер: измеренная высота приходит сообщением и перекрывает его.
pub const FALLBACK_PREVIEW_HEIGHT_PX: u32 = 720;

/// Минимальная высота кадра, которую принимает протокол.
pub const MIN_PREVIEW_HEIGHT_PX: u32 = 80;

/// Максимальная высота кадра, которую принимает протокол.
pub const MAX_PREVIEW_HEIGHT_PX: u32 = 40_000;

/// Runtime страницы отчёта: переключение темы и синхронизация высоты кадров.
pub const INDEX_RUNTIME_JS: &str = r#"
(function () {
  'use strict';
  var THEME_LIGHT = 'light';
  var THEME_NIGHT = 'night';
  var PREVIEW_CLASS = 'report-preview';
  var REPORT_NIGHT_CLASS = 'report-night';
  var THEME_ATTRIBUTE = 'data-report-theme';
  var THEME_CONTROL_ATTRIBUTE = 'data-report-theme-value';
  var HELLO_MESSAGE = 'report:hello';
  var HEIGHT_MESSAGE = 'report:height';
  var THEME_MESSAGE = 'report:theme';
  var MIN_HEIGHT = 80;
  var MAX_HEIGHT = 40000;
  var theme = THEME_LIGHT;

  function frames() {
    return document.querySelectorAll('iframe.' + PREVIEW_CLASS);
  }

  function post(frame, message) {
    if (frame && frame.contentWindow) {
      frame.contentWindow.postMessage(message, '*');
    }
  }

  function broadcast() {
    var list = frames();
    for (var i = 0; i < list.length; i += 1) {
      post(list[i], { type: THEME_MESSAGE, theme: theme });
    }
  }

  function applyTheme(next) {
    theme = next === THEME_NIGHT ? THEME_NIGHT : THEME_LIGHT;
    var night = theme === THEME_NIGHT;
    document.documentElement.setAttribute(THEME_ATTRIBUTE, theme);
    if (document.body) {
      if (night) {
        document.body.classList.add(REPORT_NIGHT_CLASS);
      } else {
        document.body.classList.remove(REPORT_NIGHT_CLASS);
      }
    }
    var options = document.querySelectorAll('[' + THEME_CONTROL_ATTRIBUTE + ']');
    for (var i = 0; i < options.length; i += 1) {
      var pressed = options[i].getAttribute(THEME_CONTROL_ATTRIBUTE) === theme;
      options[i].setAttribute('aria-pressed', pressed ? 'true' : 'false');
    }
    broadcast();
  }

  function frameOf(source) {
    var list = frames();
    for (var i = 0; i < list.length; i += 1) {
      if (list[i].contentWindow === source) {
        return list[i];
      }
    }
    return null;
  }

  function setHeight(frame, height) {
    if (!frame || typeof height !== 'number' || !isFinite(height)) {
      return;
    }
    var bounded = Math.max(MIN_HEIGHT, Math.min(MAX_HEIGHT, Math.round(height)));
    frame.style.height = bounded + 'px';
    frame.setAttribute('data-report-height', String(bounded));
  }

  function wire() {
    var options = document.querySelectorAll('[' + THEME_CONTROL_ATTRIBUTE + ']');
    for (var i = 0; i < options.length; i += 1) {
      options[i].addEventListener('click', function (event) {
        var target = event.currentTarget;
        if (target && target.getAttribute) {
          applyTheme(target.getAttribute(THEME_CONTROL_ATTRIBUTE));
        }
      });
    }
    window.addEventListener('message', function (event) {
      var data = event.data;
      if (!data || typeof data.type !== 'string') {
        return;
      }
      var frame = frameOf(event.source);
      if (data.type === HEIGHT_MESSAGE) {
        setHeight(frame, Number(data.height));
      } else if (data.type === HELLO_MESSAGE) {
        if (frame) {
          post(frame, { type: THEME_MESSAGE, theme: theme });
          setHeight(frame, Number(data.height));
        }
      }
    });
    applyTheme(theme);
  }

  wire();
})();
"#;

/// Runtime документа превью: применяет тему состояния и сообщает свою высоту.
pub const CARD_RUNTIME_JS: &str = r#"
(function () {
  'use strict';
  var NIGHT_CLASS = 'nightMode';
  var HELLO_MESSAGE = 'report:hello';
  var HEIGHT_MESSAGE = 'report:height';
  var THEME_MESSAGE = 'report:theme';
  var MIN_HEIGHT = 80;
  var HEIGHT_EPSILON = 1;
  var MAX_HEIGHT = 40000;

  function post(message) {
    if (window.parent && window.parent !== window) {
      window.parent.postMessage(message, '*');
    }
  }

  function targets() {
    var found = [document.documentElement, document.body];
    var cards = document.querySelectorAll('.card');
    for (var i = 0; i < cards.length; i += 1) {
      found.push(cards[i]);
    }
    return found;
  }

  function applyTheme(theme) {
    var night = theme === 'night';
    var list = targets();
    for (var i = 0; i < list.length; i += 1) {
      var element = list[i];
      if (!element || !element.classList) {
        continue;
      }
      if (night) {
        element.classList.add(NIGHT_CLASS);
      } else {
        element.classList.remove(NIGHT_CLASS);
      }
    }
  }

  function contentHeight() {
    var height = 0;
    var root = document.documentElement;
    var body = document.body;
    if (root) {
      height = Math.max(height, root.scrollHeight || 0, root.offsetHeight || 0);
    }
    if (body) {
      height = Math.max(height, body.scrollHeight || 0, body.offsetHeight || 0);
    }
    // Запас в один пиксель гасит дробное округление: без него кадр почти того
    // же размера, что содержимое, получает собственный scrollbar и отчёт
    // прокручивается двумя вложенными областями.
    height += HEIGHT_EPSILON;
    return Math.max(MIN_HEIGHT, Math.min(MAX_HEIGHT, height));
  }

  function reportHeight() {
    post({ type: HEIGHT_MESSAGE, height: contentHeight() });
  }

  function announce() {
    post({ type: HELLO_MESSAGE, height: contentHeight() });
  }

  function watch() {
    // Локальные media грузятся с диска и меняют высоту уже после load, поэтому
    // измерение повторяется на каждой загрузке картинки и на готовности аудио.
    document.addEventListener('load', function (event) {
      var target = event.target;
      if (!target || typeof target.tagName !== 'string') {
        return;
      }
      if (target.tagName === 'IMG' || target.tagName === 'AUDIO' || target.tagName === 'VIDEO') {
        reportHeight();
      }
    }, true);
    document.addEventListener('loadedmetadata', reportHeight, true);
    window.addEventListener('load', reportHeight);
    window.addEventListener('resize', reportHeight);
    window.addEventListener('message', function (event) {
      var data = event.data;
      if (data && data.type === THEME_MESSAGE) {
        applyTheme(data.theme);
        reportHeight();
      }
    });
    announce();
  }

  watch();
})();
"#;
