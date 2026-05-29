// RyeOS web shell bootstrap.
//
// 1. GET /ui/api/session/current
// 2. Boot cockpit (which loads snapshot + topology internally)
//
// The cockpit handles all subsequent data loading and rendering.

(function () {
  "use strict";

  if (window.RyeCockpit && window.RyeCockpit.boot) {
    window.RyeCockpit.boot();
  } else {
    var app = document.getElementById("app");
    app.innerHTML =
      '<span class="error">Cockpit failed to load. Check browser console.</span>';
  }
})();
