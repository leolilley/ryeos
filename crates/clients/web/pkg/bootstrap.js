// RyeOS web shell bootstrap.
//
// 1. GET /ui/api/session/current
// 2. POST /items/effective for session.surface_ref
// 3. EventSource(session.events_url)
// 4. Render placeholder status

(function () {
  "use strict";

  var app = document.getElementById("app");

  function renderError(msg) {
    app.innerHTML = '<span class="error">' + msg + "</span>";
  }

  function renderStatus(session, surface, connected) {
    var html = "<pre>";
    html += "Session:  " + session.session_id + "\n";
    html += "Surface:  " + session.surface_ref + "\n";
    html += "Resolved: " + (surface ? "yes" : "no") + "\n";
    html += "Events:   " + (connected ? "connected" : "disconnected") + "\n";
    html += "</pre>";
    app.innerHTML = html;
  }

  fetch("/ui/api/session/current")
    .then(function (resp) {
      if (!resp.ok) {
        throw new Error("session request failed: " + resp.status);
      }
      return resp.json();
    })
    .then(function (session) {
      // Resolve effective surface.
      var surface = null;
      fetch("/items/effective", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ item_ref: session.surface_ref }),
      })
        .then(function (r) {
          return r.ok ? r.json() : null;
        })
        .then(function (s) {
          surface = s;
          renderStatus(session, surface, false);
        });

      // Open session event stream.
      var eventsUrl = "/ui/events/session/" + session.session_id;
      var es = new EventSource(eventsUrl);

      es.onopen = function () {
        renderStatus(session, surface, true);
      };

      es.addEventListener("snapshot_required", function () {
        location.reload();
      });

      es.onerror = function () {
        renderStatus(session, surface, false);
      };

      renderStatus(session, null, false);
    })
    .catch(function (err) {
      if (
        err.message &&
        err.message.indexOf("session request failed: 40") !== -1
      ) {
        renderError("Not authenticated. Launch from: ryeos web");
      } else {
        renderError("Error: " + err.message);
      }
    });
})();
