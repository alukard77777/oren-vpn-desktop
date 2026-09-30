// MultiCore: the dashboard is read-only. Only Logs, Rules and Connections are reachable, every
// other route goes to Logs, rule toggles are hidden and so are the Connections page's close,
// close-all and block buttons. The daemon's proxy enforces the same limits on the server side
// (403 for everything else, including DELETE /connections); this file only keeps the interface
// honest.
(function () {
  'use strict';
  var ALLOWED = ['#/logs', '#/rules', '#/connections', '#/setup'];
  // Zashboard v3.29.1 draws the per-connection close and block buttons and the toolbar's
  // close-all button as unlabeled `btn-circle` buttons around an `h-4` Heroicon (x-mark or
  // no-symbol). Dialog close buttons (`btn-ghost`, aria-label) and input clear buttons (`p-0`,
  // smaller icon) stay visible.
  var ACTION_BUTTON = 'button.btn-circle:not(.btn-ghost):not(.p-0):not([aria-label]) > svg.h-4 > ';
  var CONNECTION_ACTIONS =
    ACTION_BUTTON + 'path[d^="M6 18 18 6M6 6l12 12"],' +
    ACTION_BUTTON + 'path[d^="M18.364 18.364A9 9 0 0 0 5.636 5.636"]';

  function allowed(hash) {
    return ALLOWED.some(function (prefix) {
      return hash === prefix || hash.indexOf(prefix + '?') === 0 || hash.indexOf(prefix + '/') === 0;
    });
  }

  function enforceRoute() {
    if (!allowed(window.location.hash || '')) {
      window.location.replace('#/logs');
    }
  }

  function hideForbidden() {
    var links = document.querySelectorAll('a[href^="#/"]');
    for (var i = 0; i < links.length; i++) {
      if (!allowed(links[i].getAttribute('href') || '')) {
        (links[i].closest('li') || links[i]).style.display = 'none';
      }
    }
    if ((window.location.hash || '').indexOf('#/rules') === 0) {
      var toggles = document.querySelectorAll('input[type="checkbox"], input.toggle, .toggle');
      for (var j = 0; j < toggles.length; j++) {
        toggles[j].style.display = 'none';
      }
    }
    if ((window.location.hash || '').indexOf('#/connections') === 0) {
      var icons = document.querySelectorAll(CONNECTION_ACTIONS);
      for (var k = 0; k < icons.length; k++) {
        icons[k].closest('button').style.display = 'none';
      }
    }
  }

  window.addEventListener('hashchange', function () {
    enforceRoute();
    hideForbidden();
  });
  // Logs and Connections stream constantly: hide at most once per frame, not once per
  // mutation batch.
  var pending = false;
  function scheduleHide() {
    if (pending) {
      return;
    }
    pending = true;
    window.requestAnimationFrame(function () {
      pending = false;
      hideForbidden();
    });
  }

  enforceRoute();
  new MutationObserver(scheduleHide).observe(document.documentElement, {
    childList: true,
    subtree: true,
  });
})();
