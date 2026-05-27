// RyeOS web shell bootstrap.
//
// 1. GET /ui/api/session/current
// 2. POST /items/effective for session.surface_ref
// 3. GET /ui/api/graph/topology
// 4. EventSource(session.events_url)
// 5. Render the topology graph when available, otherwise status

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
      var state = {
        session: session,
        surface: null,
        connected: false,
        topology: null,
      };

      function render() {
        if (state.topology && window.RyeGraphView) {
          window.RyeGraphView.render(app, state.topology, {
            session: state.session,
            surface: state.surface,
            connected: state.connected,
          });
        } else {
          renderStatus(state.session, state.surface, state.connected);
        }
      }

      // Resolve effective surface. This is best-effort; the graph can render
      // from topology data even if surface resolution is unavailable.
      var surface = null;
      fetch("/items/effective", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ canonical_ref: session.surface_ref }),
      })
        .then(function (r) {
          return r.ok ? r.json() : null;
        })
        .then(function (s) {
          state.surface = s;
          render();
        });

      fetch("/ui/api/graph/topology")
        .then(function (r) {
          if (!r.ok) {
            throw new Error("topology request failed: " + r.status);
          }
          return r.json();
        })
        .then(function (topology) {
          state.topology = topology;
          render();
        })
        .catch(function () {
          render();
        });

      // Open session event stream.
      var eventsUrl = "/ui/events/session/" + session.session_id;
      var es = new EventSource(eventsUrl);

      es.onopen = function () {
        state.connected = true;
        render();
      };

      es.addEventListener("snapshot_required", function () {
        location.reload();
      });

      es.onerror = function () {
        state.connected = false;
        render();
      };

      render();
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
