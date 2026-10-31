/*
 * tuliprox plugin frame bridge, ABI 1.
 *
 * Served at /plugins/_sdk/bridge.js and loaded by the plugin's own frame
 * documents. A classic ES2017 script with no imports. It never touches the
 * network: its only channel is postMessage to the tuliprox page that created
 * the frame, which performs every rpc call with its own credentials.
 *
 * Messages are plain objects { tlpx: 1, type, ... }.
 *   frame -> parent: ready, rpc {id, method, payload}, toast {severity, message},
 *                    resize {height}
 *   parent -> frame: init, locale, theme, context, rpc-result {id, ok, ...}
 * Only messages whose event.source is window.parent are read, and an rpc-result
 * only for an id this script issued.
 */
(function () {
  'use strict';

  if (window.tuliprox) {
    return;
  }

  var PROTOCOL = 1;
  var SEVERITIES = ['info', 'success', 'warning', 'error'];
  var EVENTS = ['init', 'locale', 'theme', 'context'];
  var LOCALE_PATTERN = /^[A-Za-z0-9_-]{1,35}$/;
  var THEME_NAME_PATTERN = /^--[A-Za-z0-9_-]{1,100}$/;
  var PLACEHOLDER_PATTERN = /\{([A-Za-z0-9_.-]+)\}/g;

  var hosted = window.parent !== window;
  var hasOwn = Object.prototype.hasOwnProperty;

  // The slot is in the query string and the plugin id in the path until init
  // delivers the authoritative values.
  var initialSlot = '';
  var initialPluginId = '';
  try {
    var query = /(?:^|[?&])slot=([^&#]*)/.exec(window.location.search);
    initialSlot = query ? decodeURIComponent(query[1]) : '';
    var segments = /\/plugins\/([^/]+)\/ui\//.exec(window.location.pathname);
    initialPluginId = segments ? decodeURIComponent(segments[1]) : '';
  } catch (ignored) {
    initialSlot = '';
    initialPluginId = '';
  }

  var state = {
    pluginId: initialPluginId,
    slot: initialSlot,
    locale: 'en',
    dir: 'ltr',
    strings: {},
    theme: {},
    context: null
  };

  var listeners = { init: [], locale: [], theme: [], context: [] };
  var pending = new Map();
  var nextId = 0;
  var initialized = false;
  var resolveReady;
  var ready = new Promise(function (resolve) {
    resolveReady = resolve;
  });
  var appliedTheme = [];

  function post(message) {
    if (!hosted) {
      return;
    }
    message.tlpx = PROTOCOL;
    window.parent.postMessage(message, '*');
  }

  function isObject(value) {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
  }

  function emit(event) {
    var callbacks = listeners[event].slice();
    for (var index = 0; index < callbacks.length; index += 1) {
      try {
        callbacks[index]();
      } catch (error) {
        if (window.console && typeof window.console.error === 'function') {
          window.console.error('tuliprox: a ' + event + ' listener threw', error);
        }
      }
    }
  }

  function applyDocumentLanguage() {
    var root = document.documentElement;
    root.setAttribute('lang', state.locale);
    root.setAttribute('dir', state.dir);
  }

  function applyTheme() {
    var style = document.documentElement.style;
    var names = Object.keys(state.theme);
    for (var index = 0; index < appliedTheme.length; index += 1) {
      if (!hasOwn.call(state.theme, appliedTheme[index])) {
        style.removeProperty(appliedTheme[index]);
      }
    }
    appliedTheme = names;
    for (var next = 0; next < names.length; next += 1) {
      style.setProperty(names[next], state.theme[names[next]]);
    }
  }

  // Keep only well-formed entries: a custom property name and a string value.
  function cleanTheme(theme) {
    var clean = {};
    if (!isObject(theme)) {
      return clean;
    }
    Object.keys(theme).forEach(function (name) {
      if (THEME_NAME_PATTERN.test(name) && typeof theme[name] === 'string') {
        clean[name] = theme[name];
      }
    });
    return clean;
  }

  function cleanStrings(strings) {
    var clean = {};
    if (!isObject(strings)) {
      return clean;
    }
    Object.keys(strings).forEach(function (key) {
      if (typeof strings[key] === 'string') {
        clean[key] = strings[key];
      }
    });
    return clean;
  }

  function setLocale(message) {
    if (typeof message.locale === 'string' && LOCALE_PATTERN.test(message.locale)) {
      state.locale = message.locale;
    }
    state.dir = message.dir === 'rtl' ? 'rtl' : 'ltr';
    state.strings = cleanStrings(message.strings);
    applyDocumentLanguage();
  }

  function setTheme(theme) {
    state.theme = cleanTheme(theme);
    applyTheme();
  }

  function settleRpc(message) {
    if (typeof message.id !== 'number' || !pending.has(message.id)) {
      return;
    }
    var entry = pending.get(message.id);
    pending.delete(message.id);
    if (message.ok === true) {
      entry.resolve(message.result);
    } else {
      entry.reject({
        status: typeof message.status === 'number' ? message.status : 0,
        error: typeof message.error === 'string' ? message.error : 'the call failed'
      });
    }
  }

  function onMessage(event) {
    if (event.source !== window.parent) {
      return;
    }
    var message = event.data;
    if (!isObject(message) || message.tlpx !== PROTOCOL || typeof message.type !== 'string') {
      return;
    }
    switch (message.type) {
      case 'init':
        if (typeof message.pluginId === 'string') {
          state.pluginId = message.pluginId;
        }
        if (typeof message.slot === 'string') {
          state.slot = message.slot;
        }
        setLocale(message);
        setTheme(message.theme);
        state.context = hasOwn.call(message, 'context') ? message.context : null;
        if (!initialized) {
          initialized = true;
          resolveReady();
        }
        emit('init');
        break;
      case 'locale':
        setLocale(message);
        emit('locale');
        break;
      case 'theme':
        setTheme(message.theme);
        emit('theme');
        break;
      case 'context':
        state.context = hasOwn.call(message, 'context') ? message.context : null;
        emit('context');
        break;
      case 'rpc-result':
        settleRpc(message);
        break;
      default:
        break;
    }
  }

  function rpc(method, payload) {
    if (typeof method !== 'string' || method === '') {
      return Promise.reject({ status: 0, error: 'the method must be a non-empty string' });
    }
    if (!hosted) {
      return Promise.reject({ status: 0, error: 'this document is not inside a tuliprox frame' });
    }
    var body = null;
    if (payload !== undefined) {
      try {
        body = JSON.parse(JSON.stringify(payload));
      } catch (error) {
        return Promise.reject({ status: 0, error: 'the payload is not JSON-serializable' });
      }
      if (body === undefined) {
        body = null;
      }
    }
    nextId += 1;
    var id = nextId;
    return new Promise(function (resolve, reject) {
      pending.set(id, { resolve: resolve, reject: reject });
      post({ type: 'rpc', id: id, method: method, payload: body });
    });
  }

  function toast(severity, message) {
    post({
      type: 'toast',
      severity: SEVERITIES.indexOf(severity) >= 0 ? severity : 'info',
      message: String(message)
    });
  }

  function t(key, params) {
    var text = hasOwn.call(state.strings, key) ? state.strings[key] : key;
    if (!isObject(params)) {
      return text;
    }
    return text.replace(PLACEHOLDER_PATTERN, function (whole, name) {
      return hasOwn.call(params, name) ? String(params[name]) : whole;
    });
  }

  function resize(height) {
    var value = height === undefined ? document.documentElement.scrollHeight : Number(height);
    if (!isFinite(value) || value < 0) {
      return;
    }
    post({ type: 'resize', height: Math.ceil(value) });
  }

  function on(event, callback) {
    if (EVENTS.indexOf(event) < 0 || typeof callback !== 'function') {
      return function () {};
    }
    listeners[event].push(callback);
    return function () {
      var index = listeners[event].indexOf(callback);
      if (index >= 0) {
        listeners[event].splice(index, 1);
      }
    };
  }

  // The frame's height follows its content. getBoundingClientRect of the root
  // shrinks with the content, where scrollHeight never drops below the frame's
  // current height.
  function watchSize() {
    if (typeof ResizeObserver !== 'function' || !hosted) {
      return;
    }
    var scheduled = false;
    var last = -1;
    var observer = new ResizeObserver(function () {
      if (scheduled) {
        return;
      }
      scheduled = true;
      window.requestAnimationFrame(function () {
        scheduled = false;
        var height = Math.ceil(document.documentElement.getBoundingClientRect().height);
        if (height !== last) {
          last = height;
          post({ type: 'resize', height: height });
        }
      });
    });
    observer.observe(document.documentElement);
    if (document.body) {
      observer.observe(document.body);
    }
  }

  var api = {
    get ready() {
      return ready;
    },
    get pluginId() {
      return state.pluginId;
    },
    get slot() {
      return state.slot;
    },
    get locale() {
      return state.locale;
    },
    get dir() {
      return state.dir;
    },
    get theme() {
      return state.theme;
    },
    get context() {
      return state.context;
    },
    rpc: rpc,
    toast: toast,
    t: t,
    resize: resize,
    on: on
  };

  Object.defineProperty(window, 'tuliprox', { value: Object.freeze(api), enumerable: true });

  if (!hosted) {
    return;
  }
  window.addEventListener('message', onMessage);
  post({ type: 'ready' });
  // The parent answers every ready with init. Ask again once the document is
  // loaded if it has not answered yet, in case it attached its listener late.
  window.addEventListener('load', function () {
    if (!initialized) {
      post({ type: 'ready' });
    }
    watchSize();
  });
  if (document.readyState === 'complete') {
    watchSize();
  }
})();
