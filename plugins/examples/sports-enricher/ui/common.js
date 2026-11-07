// Shared helpers of the Sports Enricher frames. The frame CSP is `script-src 'self'`, so there are no
// inline scripts, and `connect-src 'none'`: data only arrives through `tuliprox.rpc`.
// Everything from the API is written with textContent, never innerHTML.
(function () {
  'use strict';

  var tuliprox = window.tuliprox;
  var POLL_MS = 10000;
  var EMOJI = {
    football: '⚽', basketball: '🏀', 'american-football': '🏈',
    'ice-hockey': '🏒', baseball: '⚾', tennis: '🎾', rugby: '🏉',
    cricket: '🏏', golf: '⛳', motorsport: '🏎️', combat: '🥊'
  };

  function el(tag, attrs, children) {
    var node = document.createElement(tag);
    Object.keys(attrs || {}).forEach(function (name) {
      if (name === 'text') node.textContent = attrs[name];
      else if (name === 'class') node.className = attrs[name];
      else node.setAttribute(name, attrs[name]);
    });
    (children || []).forEach(function (child) { node.appendChild(child); });
    return node;
  }

  function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }

  // Unix seconds as a UTC date and time in the frame's locale. Fixture times are UTC; so is the label.
  function formatTime(unix) {
    var date = new Date(unix * 1000);
    try {
      return date.toLocaleString(tuliprox.locale, {
        dateStyle: 'medium', timeStyle: 'short', timeZone: 'UTC'
      }) + ' UTC';
    } catch (ignored) {
      return date.toISOString().replace('T', ' ').slice(0, 16) + ' UTC';
    }
  }

  // live | upcoming | finished, against the host clock the rpc returned.
  function status(fixture, now) {
    var end = fixture.kickoff + (fixture.duration_min || 120) * 60;
    if (now >= end) return 'finished';
    return now >= fixture.kickoff ? 'live' : 'upcoming';
  }

  function sportEmoji(sport) { return EMOJI[sport] || '🏅'; }

  function load(payload) { return tuliprox.rpc('get-fixtures', payload || {}); }

  function describeError(error) {
    if (error && typeof error === 'object' && 'error' in error) return String(error.error);
    return String(error);
  }

  // Calls `render(view)` now and every POLL_MS. When the host reports a newer fixtures refresh or
  // playlist update than the one first seen, shows the "Fixtures refreshed" toast (`options.toast`).
  // The bridge has no server push, so polling the rpc is how a frame learns about a refresh.
  function watch(request, render, fail, options) {
    var seen = null;
    function stamp(view) {
      return Math.max(view.refreshed_at || 0, view.last_update ? view.last_update.at : 0);
    }
    function tick() {
      load(typeof request === 'function' ? request() : request).then(function (view) {
        var current = stamp(view);
        if (seen !== null && current > seen && options && options.toast) {
          tuliprox.toast('success', tuliprox.t('toast.refreshed'));
        }
        seen = seen === null ? current : Math.max(seen, current);
        render(view);
        tuliprox.resize();
      }, function (error) {
        fail(describeError(error));
        tuliprox.resize();
      });
    }
    tick();
    var timer = window.setInterval(tick, POLL_MS);
    return { reload: tick, stop: function () { window.clearInterval(timer); } };
  }

  window.sportsCommon = {
    el: el, clear: clear, formatTime: formatTime, status: status, sportEmoji: sportEmoji,
    load: load, watch: watch, describeError: describeError
  };
})();
