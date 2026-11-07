// Source-editor tile: what the plugin cached and what it did on the last playlist refresh.
(async function () {
  'use strict';

  var tuliprox = window.tuliprox;
  var c = window.sportsCommon;
  var $ = function (id) { return document.getElementById(id); };
  var view = null;
  var watcher = null;

  await tuliprox.ready;

  function fact(key, value) {
    var list = $('facts');
    list.appendChild(c.el('dt', { text: tuliprox.t(key) }));
    list.appendChild(c.el('dd', { text: String(value) }));
  }

  function render(next) {
    view = next;
    document.title = tuliprox.t('tile.title');
    $('heading').textContent = tuliprox.t('tile.title');
    $('reload').textContent = tuliprox.t('guide.reload');
    c.clear($('facts'));
    var box = $('message');
    box.hidden = view.configured;
    if (!view.configured) {
      box.textContent = tuliprox.t('guide.not_configured');
      box.className = 'notice warn';
    }
    fact('tile.leagues', view.leagues.map(function (league) { return league.name; }).join(', ') || '-');
    fact('tile.fixtures', view.leagues.reduce(function (sum, league) { return sum + league.fixtures; }, 0));
    fact('tile.api_calls', view.calls_today);
    fact('tile.last_fixture_refresh', view.refreshed_at ? c.formatTime(view.refreshed_at) : tuliprox.t('guide.never_refreshed'));
    var update = view.last_update;
    fact('tile.last_playlist_update', update ? c.formatTime(update.at) + ' (' + update.state + ')' : '-');
    if (update) {
      fact('tile.channels_enriched', update.regrouped + ' / ' + update.sport_channels);
      fact('tile.programmes_added', update.programmes_added);
      fact('tile.vod_enriched', update.vod_enriched);
    }
  }

  function fail(error) {
    var box = $('message');
    box.textContent = tuliprox.t('guide.error', { error: error });
    box.className = 'notice warn';
    box.hidden = false;
  }

  $('reload').addEventListener('click', function () { if (watcher) watcher.reload(); });
  watcher = c.watch({ limit: 1 }, render, fail, { toast: true });
  for (const event of ['locale', 'theme']) {
    tuliprox.on(event, function () { if (view) render(view); tuliprox.resize(); });
  }
})();
