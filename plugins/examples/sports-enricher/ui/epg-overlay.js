// EPG overlay: facts about the focused programme when it is one of the plugin's fixtures.
// Context (see @tuliprox/plugin-ui-types): { channel: {epgChannelId, ...}, programme: {title, start, stop, ...} | null }.
(async function () {
  'use strict';

  var tuliprox = window.tuliprox;
  var c = window.sportsCommon;
  var $ = function (id) { return document.getElementById(id); };
  var watcher = null;
  var lastChannel = null;

  await tuliprox.ready;

  function context() {
    var ctx = tuliprox.context || {};
    return { channel: ctx.channel || null, programme: ctx.programme || null };
  }

  function note(text, warn) {
    var box = $('message');
    box.textContent = text;
    box.className = warn ? 'notice warn' : 'notice';
    box.hidden = false;
    c.clear($('facts'));
  }

  function fact(label, value) {
    $('facts').appendChild(c.el('dt', { text: label }));
    $('facts').appendChild(c.el('dd', { text: value }));
  }

  // The fixture the focused programme is: same "Home vs Away" title (any case). With no programme
  // focused, the channel's next fixture.
  function pick(view, programme) {
    if (!programme) return view.fixtures[0] || null;
    var wanted = String(programme.title || '').trim().toLowerCase();
    return view.fixtures.find(function (fixture) { return fixture.title.toLowerCase() === wanted; }) || null;
  }

  function render(view) {
    document.title = tuliprox.t('overlay.title');
    $('heading').textContent = tuliprox.t('overlay.title');
    var ctx = context();
    var fixture = pick(view, ctx.programme);
    if (!fixture) {
      note(tuliprox.t(view.configured ? 'overlay.no_match' : 'guide.not_configured'), !view.configured);
      return;
    }
    $('message').hidden = true;
    c.clear($('facts'));
    fact(tuliprox.t('overlay.match'), c.sportEmoji(fixture.sport) + ' ' + fixture.title);
    fact(tuliprox.t('overlay.league'), fixture.league_name + (fixture.round ? ' (' + tuliprox.t('overlay.round', { round: fixture.round }) + ')' : ''));
    fact(tuliprox.t('overlay.kickoff'), c.formatTime(fixture.kickoff));
    fact(tuliprox.t('overlay.status'), tuliprox.t('status.' + c.status(fixture, view.now)));
    if (fixture.venue) fact(tuliprox.t('overlay.venue'), fixture.venue);
    Object.keys(fixture.stats || {}).sort().forEach(function (key) {
      fact(tuliprox.t('stat.' + key), fixture.stats[key]);
    });
  }

  function fail(error) { note(tuliprox.t('guide.error', { error: error }), true); }

  function start() {
    if (watcher) watcher.stop();
    var ctx = context();
    var channel = ctx.channel && ctx.channel.epgChannelId;
    lastChannel = channel || null;
    if (!channel) {
      note(tuliprox.t('overlay.no_channel'), false);
      tuliprox.resize();
      return;
    }
    watcher = c.watch({ channel: channel, limit: 20 }, render, fail, { toast: false });
  }

  start();
  tuliprox.on('context', function () {
    var ctx = context();
    var channel = ctx.channel && ctx.channel.epgChannelId;
    if ((channel || null) !== lastChannel) start();
    else if (watcher) watcher.reload();
  });
  for (const event of ['locale', 'theme']) {
    tuliprox.on(event, function () { if (watcher) watcher.reload(); });
  }
})();
