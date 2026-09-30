// MultiCore: the dashboard is read-only. Only Logs and Rules are reachable, every other route
// goes to Logs, and rule toggles are hidden. The daemon's proxy enforces the same limits on the
// server side (403 for everything else); this file only keeps the interface honest.
(function () {
  'use strict';
  var ALLOWED = ['#/logs', '#/rules', '#/setup'];

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
  }

  window.addEventListener('hashchange', function () {
    enforceRoute();
    hideForbidden();
  });
  // Logs stream constantly: hide at most once per frame, not once per mutation batch.
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
