// "Sports Guide": the menu page. Lists the upcoming fixtures of the tracked leagues.
(async function () {
  'use strict';

  var tuliprox = window.tuliprox;
  var c = window.sportsCommon;
  var $ = function (id) { return document.getElementById(id); };
  var view = null;
  var watcher = null;

  await tuliprox.ready;

  function message(text, warn) {
    var box = $('message');
    box.textContent = text;
    box.className = warn ? 'notice warn' : 'notice';
    box.hidden = false;
  }

  function renderStatic() {
    document.title = tuliprox.t('menu.title');
    $('heading').textContent = tuliprox.t('guide.heading');
    $('league-label').textContent = tuliprox.t('guide.league');
    $('reload').textContent = tuliprox.t('guide.reload');
    var head = $('head');
    c.clear(head);
    ['guide.col.kickoff', 'guide.col.match', 'guide.col.league', 'guide.col.venue', 'guide.col.status'].forEach(function (key) {
      head.appendChild(c.el('th', { text: tuliprox.t(key) }));
    });
  }

  function renderLeagues() {
    var select = $('league');
    var chosen = select.value;
    c.clear(select);
    select.appendChild(c.el('option', { value: '', text: tuliprox.t('guide.all_leagues') }));
    view.leagues.forEach(function (league) {
      select.appendChild(c.el('option', { value: league.id, text: league.name }));
    });
    select.value = chosen;
  }

  function renderRows() {
    var body = $('body');
    c.clear(body);
    var chosen = $('league').value;
    var rows = view.fixtures.filter(function (fixture) { return !chosen || fixture.league === chosen; });
    rows.forEach(function (fixture) {
      var state = c.status(fixture, view.now);
      var pill = c.el('span', { class: state === 'live' ? 'pill live' : 'pill', text: tuliprox.t('status.' + state) });
      body.appendChild(c.el('tr', {}, [
        c.el('td', { text: c.formatTime(fixture.kickoff) }),
        c.el('td', { text: c.sportEmoji(fixture.sport) + ' ' + fixture.title }),
        c.el('td', { text: fixture.league_name }),
        c.el('td', { text: fixture.venue || '' }),
        c.el('td', {}, [pill])
      ]));
    });
    $('table').hidden = rows.length === 0;
    if (!view.configured) message(tuliprox.t('guide.not_configured'), true);
    else if (rows.length === 0) message(tuliprox.t('guide.empty'), false);
    else $('message').hidden = true;
    $('updated').textContent = view.refreshed_at
      ? tuliprox.t('guide.refreshed_at', { time: c.formatTime(view.refreshed_at) })
      : tuliprox.t('guide.never_refreshed');
  }

  function render(next) {
    view = next;
    renderLeagues();
    renderRows();
  }

  function fail(error) { message(tuliprox.t('guide.error', { error: error }), true); }

  renderStatic();
  $('league').addEventListener('change', function () { if (view) { renderRows(); tuliprox.resize(); } });
  $('reload').addEventListener('click', function () { if (watcher) watcher.reload(); });
  watcher = c.watch({ limit: 100 }, render, fail, { toast: true });
  for (const event of ['locale', 'theme']) {
    tuliprox.on(event, function () { renderStatic(); if (view) render(view); tuliprox.resize(); });
  }
})();
