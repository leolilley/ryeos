// RyeOS Cockpit — 2D operational inspector.
//
// Replaces the topology-first experience with a tabbed operational view.
// The topology graph remains available as a secondary tab.
//
// Tabs: Overview | Items | Topology

(function () {
  "use strict";

  // ── State ────────────────────────────────────────────────────────

  var state = {
    session: null,
    snapshot: null,
    snapshotError: null,
    topology: null,
    topologyLoading: false,
    topologyError: null,
    surface: null,
    connected: false,
    activeTab: "overview",
    // Items tab state
    items: null,
    itemsLoading: false,
    itemsError: null,
    itemsFilter: { kind: "", space: "", query: "" },
    // Threads tab state
    threads: null,
    threadsLoading: false,
    threadsError: null,
    // Schedules tab state
    schedules: null,
    schedulesLoading: false,
    schedulesError: null,
    // GC tab state
    gcStatus: null,
    gcLoading: false,
    gcError: null,
    // Remotes tab state
    remotes: null,
    remotesLoading: false,
    remotesError: null,
    remotesProbeError: null,
    // Files tab state
    files: null,
    filesLoading: false,
    filesError: null,
    readFileError: null,
    filesRoot: "project",
    filesPath: "",
    // Inspector state
    inspectedItem: null,
    inspectLoading: false,
    inspectError: null,
  };

  // ── Entry point ──────────────────────────────────────────────────

  function boot() {
    fetch("/ui/api/session/current")
      .then(function (r) {
        if (!r.ok) throw new Error("session: " + r.status);
        return r.json();
      })
      .then(function (session) {
        state.session = session;
        render();

        // Load snapshot (main data for overview)
        fetch("/ui/api/cockpit/snapshot")
          .then(function (r) {
            if (!r.ok) throw new Error("snapshot: " + r.status);
            return r.json();
          })
          .then(function (snap) {
            state.snapshot = snap;
            state.snapshotError = null;
            render();
          })
          .catch(function (err) {
            state.snapshotError = err.message || "Failed to load snapshot";
            render();
          });

        // Load surface (best-effort)
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
          })
          .catch(function () {
            /* surface is optional for cockpit */
          });

        // Topology is loaded lazily on first Topology tab activation.

        // Open SSE
        openEvents();
      })
      .catch(function (err) {
        var app = document.getElementById("app");
        if (
          err.message &&
          err.message.indexOf("session: 40") !== -1
        ) {
          app.innerHTML =
            '<span class="error">Not authenticated. Launch from: ryeos web</span>';
        } else {
          app.innerHTML = '<span class="error">Error: ' + esc(err.message || "unknown error") + "</span>";
        }
      });
  }

  function loadTopology() {
    if (state.topologyLoading) return;
    state.topologyLoading = true;
    state.topologyError = null;

    fetch("/ui/api/graph/topology")
      .then(function (r) {
        if (!r.ok) throw new Error("topology: " + r.status);
        return r.json();
      })
      .then(function (topology) {
        state.topology = topology;
        state.topologyLoading = false;
        render();
      })
      .catch(function (err) {
        state.topologyLoading = false;
        state.topologyError = err.message || "Failed to load topology";
        render();
      });
  }

  function loadItems() {
    if (state.itemsLoading) return;
    state.itemsLoading = true;
    state.itemsError = null;

    var params = [];
    var f = state.itemsFilter;
    if (f.kind) params.push("kind=" + encodeURIComponent(f.kind));
    if (f.space) params.push("space=" + encodeURIComponent(f.space));
    if (f.query) params.push("query=" + encodeURIComponent(f.query));
    var qs = params.length ? "?" + params.join("&") : "";

    fetch("/ui/api/cockpit/items/list" + qs)
      .then(function (r) {
        if (!r.ok) throw new Error("items: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.items = data;
        state.itemsLoading = false;
        render();
      })
      .catch(function (err) {
        state.itemsLoading = false;
        state.itemsError = err.message || "Failed to load items";
        render();
      });
  }

  function loadThreads() {
    if (state.threadsLoading) return;
    state.threadsLoading = true;
    state.threadsError = null;

    fetch("/ui/api/cockpit/threads/list?limit=100")
      .then(function (r) {
        if (!r.ok) throw new Error("threads: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.threads = data;
        state.threadsLoading = false;
        render();
      })
      .catch(function (err) {
        state.threadsLoading = false;
        state.threadsError = err.message || "Failed to load threads";
        render();
      });
  }

  function loadSchedules() {
    if (state.schedulesLoading) return;
    state.schedulesLoading = true;
    state.schedulesError = null;

    fetch("/ui/api/cockpit/schedules/list")
      .then(function (r) {
        if (!r.ok) throw new Error("schedules: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.schedules = data;
        state.schedulesLoading = false;
        render();
      })
      .catch(function (err) {
        state.schedulesLoading = false;
        state.schedulesError = err.message || "Failed to load schedules";
        render();
      });
  }

  function loadGcStatus() {
    if (state.gcLoading) return;
    state.gcLoading = true;
    state.gcError = null;

    fetch("/ui/api/cockpit/gc/status")
      .then(function (r) {
        if (!r.ok) throw new Error("gc: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.gcStatus = data;
        state.gcLoading = false;
        render();
      })
      .catch(function (err) {
        state.gcLoading = false;
        state.gcError = err.message || "Failed to load GC status";
        render();
      });
  }

  function loadRemotes() {
    if (state.remotesLoading) return;
    state.remotesLoading = true;
    state.remotesError = null;

    fetch("/ui/api/cockpit/remotes/list")
      .then(function (r) {
        if (!r.ok) throw new Error("remotes: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.remotes = data;
        state.remotesLoading = false;
        render();
      })
      .catch(function (err) {
        state.remotesLoading = false;
        state.remotesError = err.message || "Failed to load remotes";
        render();
      });
  }

  function probeRemote(name) {
    fetch("/ui/api/cockpit/remotes/probe", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ remote: name }),
    })
      .then(function (r) {
        if (!r.ok) throw new Error("probe: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.remotesProbeResult = data;
        state.remotesProbeName = name;
        state.remotesProbeError = null;
        render();
      })
      .catch(function (err) {
        state.remotesProbeResult = null;
        state.remotesProbeName = name;
        state.remotesProbeError = err.message || "Failed to probe remote";
        render();
      });
  }

  function loadFiles(root, path) {
    if (state.filesLoading) return;
    state.filesLoading = true;
    state.filesError = null;
    state.filesRoot = root || "project";
    state.filesPath = path || "";

    fetch("/ui/api/cockpit/files/list", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ root: state.filesRoot, path: state.filesPath }),
    })
      .then(function (r) {
        if (!r.ok) throw new Error("files: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.files = data;
        state.filesLoading = false;
        render();
      })
      .catch(function (err) {
        state.filesLoading = false;
        state.filesError = err.message || "Failed to load files";
        render();
      });
  }

  function readFile(root, path) {
    if (state.readFileLoading) return;
    state.readFileLoading = true;
    state.readFileError = null;

    fetch("/ui/api/cockpit/files/read", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ root: root, path: path }),
    })
      .then(function (r) {
        if (!r.ok) throw new Error("read: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.readFileResult = data;
        state.readFileLoading = false;
        render();
      })
      .catch(function (err) {
        state.readFileLoading = false;
        state.readFileError = err.message || "Failed to read file";
        render();
      });
  }

  function inspectItem(canonicalRef) {
    if (state.inspectLoading) return;
    state.inspectLoading = true;
    state.inspectedItem = null;
    state.inspectError = null;
    render();

    fetch("/ui/api/cockpit/item/inspect", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        canonical_ref: canonicalRef,
        include_raw: true,
        include_effective: true,
      }),
    })
      .then(function (r) {
        if (!r.ok) throw new Error("inspect: " + r.status);
        return r.json();
      })
      .then(function (data) {
        state.inspectedItem = data;
        state.inspectLoading = false;
        render();
      })
      .catch(function (err) {
        state.inspectLoading = false;
        state.inspectError = err.message || "Failed to inspect item";
        render();
      });
  }

  function openEvents() {
    var es = new EventSource(
      "/ui/events/session/" + state.session.session_id
    );
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
  }

  // ── Render ───────────────────────────────────────────────────────

  function render() {
    var app = document.getElementById("app");
    if (!state.session) {
      app.innerHTML = '<span class="status">Loading...</span>';
      return;
    }

    // Load items on first visit to items tab
    if (state.activeTab === "items" && !state.items && !state.itemsLoading && !state.itemsError) {
      loadItems();
    }

    // Load threads on first visit to threads tab
    if (state.activeTab === "threads" && !state.threads && !state.threadsLoading && !state.threadsError) {
      loadThreads();
    }

    // Load schedules on first visit to schedules tab
    if (state.activeTab === "schedules" && !state.schedules && !state.schedulesLoading && !state.schedulesError) {
      loadSchedules();
    }

    // Load GC on first visit to gc tab
    if (state.activeTab === "gc" && !state.gcStatus && !state.gcLoading && !state.gcError) {
      loadGcStatus();
    }

    // Load remotes on first visit to remotes tab
    if (state.activeTab === "remotes" && !state.remotes && !state.remotesLoading && !state.remotesError) {
      loadRemotes();
    }

    // Load files on first visit to files tab
    if (state.activeTab === "files" && !state.files && !state.filesLoading && !state.filesError) {
      loadFiles("project", "");
    }

    // Load topology on first visit to topology tab
    if (state.activeTab === "topology" && !state.topology && !state.topologyLoading && !state.topologyError) {
      loadTopology();
    }

    var html = "";
    html += '<div class="cockpit-shell">';

    // ── Nav sidebar ──
    html += renderNav();

    // ── Main content ──
    html += '<div class="cockpit-main">';
    html += renderMainHeader();
    html += '<div class="cockpit-main-body">';
    html += renderTabPanel("overview", renderOverview);
    html += renderTabPanel("items", renderItems);
    html += renderTabPanel("threads", renderThreads);
    html += renderTabPanel("schedules", renderSchedules);
    html += renderTabPanel("gc", renderGc);
    html += renderTabPanel("remotes", renderRemotes);
    html += renderTabPanel("files", renderFiles);
    html += renderTabPanel("topology", renderTopology);
    html += "</div>"; // main-body
    html += "</div>"; // main

    // ── Inspector ──
    html += '<div class="cockpit-inspector">';
    html += renderInspector();
    html += "</div>";

    html += "</div>"; // shell

    app.innerHTML = html;
    bindEvents();

    // If topology tab is active, render the 3D graph
    if (state.activeTab === "topology" && state.topology && window.RyeGraphView) {
      requestAnimationFrame(function () {
        var container = document.getElementById("cockpit-topology-graph");
        if (container) {
          window.RyeGraphView.render(container, state.topology, {
            session: state.session,
            surface: state.surface,
            connected: state.connected,
          });
        }
      });
    }
  }

  // ── Nav ──────────────────────────────────────────────────────────

  function renderNav() {
    var h = "";
    h += '<nav class="cockpit-nav">';
    h += '<div class="cockpit-nav-header">Rye OS · Cockpit</div>';
    h += '<ul class="cockpit-nav-list">';
    h += navItem("overview", "Overview", true);
    h += navItem("items", "Items");
    h += navItem("threads", "Threads");
    h += navItem("schedules", "Schedules");
    h += navItem("gc", "GC");
    h += navItem("remotes", "Remotes");
    h += navItem("files", "Files");
    h += navItem("topology", "Topology");
    h += "</ul>";
    h += '<div class="cockpit-nav-footer">';
    h += sessionStatusText();
    h += "</div>";
    h += "</nav>";
    return h;
  }

  function navItem(tab, label, primary) {
    var cls = "cockpit-nav-item";
    if (state.activeTab === tab) cls += " active";
    var badge = primary ? ' <span style="color:var(--orange)">●</span>' : "";
    return (
      '<li><button class="' + attrEsc(cls) + '" data-tab="' + attrEsc(tab) + '">' +
      label + badge + "</button></li>"
    );
  }

  function sessionStatusText() {
    var parts = [];
    if (state.snapshot) {
      parts.push(state.snapshot.local_node.identity.principal_id.slice(0, 12) + "…");
      parts.push(state.snapshot.local_node.health.status);
    }
    if (state.connected) parts.push("SSE");
    else parts.push("OFFLINE");
    return parts.join(" · ");
  }

  // ── Main header ──────────────────────────────────────────────────

  function renderMainHeader() {
    var titles = { overview: "Overview", items: "Items", threads: "Threads", schedules: "Schedules", gc: "GC", remotes: "Remotes", files: "Files", topology: "Topology Graph" };
    var h = "";
    h += '<div class="cockpit-main-header">';
    h +=
      '<div class="cockpit-main-title">' +
      (titles[state.activeTab] || "Overview") +
      "</div>";
    if (state.snapshot) {
      h +=
        '<div class="cockpit-main-meta">' +
        esc(state.snapshot.generated_at) +
        "</div>";
    }
    h += "</div>";
    return h;
  }

  function renderTabPanel(tab, renderFn) {
    var cls = "cockpit-tab-panel";
    if (state.activeTab === tab) cls += " active";
    return '<div class="' + attrEsc(cls) + '" id="tab-' + attrEsc(tab) + '">' + renderFn() + "</div>";
  }

  // ── Overview tab ──────────────────────────────────────────────────

  function renderOverview() {
    if (state.snapshotError) {
      return renderErrorState("Failed to load overview snapshot", state.snapshotError);
    }

    if (!state.snapshot) {
      return '<div class="cockpit-empty">Loading snapshot…</div>';
    }

    var snap = state.snapshot;
    var localNode = snap.local_node || {};
    var spaces = localNode.spaces || [];
    var bundles = localNode.bundles || [];
    var remotes = snap.remotes || [];
    var services = localNode.services || [];
    var verbs = localNode.verbs || [];
    var aliases = localNode.aliases || [];
    var h = "";

    h += renderStatusCards(snap);

    h += section("Resolution Spaces", function () {
      if (spaces.length === 0)
        return '<div class="cockpit-empty">No spaces found</div>';
      return table(
        ["Space", "Label", "Path"],
        spaces.map(function (s) {
          return [
            tag(s.space),
            esc(s.label),
            '<span class="cockpit-card-value path">' + esc(s.path) + "</span>",
          ];
        })
      );
    });

    h += section("Installed Bundles", function () {
      if (bundles.length === 0)
        return '<div class="cockpit-empty">No bundles installed</div>';
      return table(
        ["Bundle", "Path"],
        bundles.map(function (b) {
          return [esc(b.name), '<span class="cockpit-card-value path">' + esc(b.path) + "</span>"];
        })
      );
    });

    h += section("Remotes (configured)", function () {
      if (remotes.length === 0)
        return '<div class="cockpit-empty">No remotes configured</div>';
      return table(
        ["Name", "URL", "Principal"],
        remotes.map(function (r) {
          return [esc(r.name), esc(r.url), esc(r.principal_id.slice(0, 16) + "…")];
        })
      );
    });

    h += section("Services (" + services.length + ")", function () {
      if (services.length === 0)
        return '<div class="cockpit-empty">No services registered</div>';
      return table(
        ["Endpoint", "Service Ref", "Availability"],
        services.map(function (s) {
          return [esc(s.endpoint), esc(s.service_ref), esc(s.availability)];
        })
      );
    });

    h += section("Verbs (" + verbs.length + ")", function () {
      if (verbs.length === 0)
        return '<div class="cockpit-empty">No verbs registered</div>';
      return table(
        ["Verb", "Target"],
        verbs.map(function (v) {
          return [esc(v.name), v.target ? esc(v.target) : "—"];
        })
      );
    });

    h += section("Aliases (" + aliases.length + ")", function () {
      if (aliases.length === 0)
        return '<div class="cockpit-empty">No aliases registered</div>';
      return table(
        ["Alias", "Target"],
        aliases.map(function (a) {
          return [esc(a.name), a.target ? esc(a.target) : "—"];
        })
      );
    });

    h += section("Garbage Collection", function () {
      var gc = snap.gc || {};
      var recentEvents = gc.recent_events || [];
      var h2 = "";
      h2 += '<div class="cockpit-card" style="margin-bottom:0.5rem">';
      h2 += '<div class="cockpit-card-label">Status</div>';
      h2 +=
        '<div class="cockpit-card-value ' +
        (gc.running ? "status-running" : "status-idle") +
        '">' +
        (gc.running ? "● RUNNING" : "○ Idle") +
        "</div>";
      h2 += "</div>";

      if (recentEvents.length > 0) {
        h2 += table(
          ["Timestamp", "Freed", "Duration"],
          recentEvents.map(function (e) {
            return [
              esc(e.timestamp || ""),
              formatBytes(e.freed_bytes || 0),
              (e.duration_ms || 0) + "ms",
            ];
          })
        );
      }
      return h2;
    });

    return h;
  }

  function renderStatusCards(snap) {
    var node = snap.local_node || {};
    var health = node.health || {};
    var status = node.status || {};
    var threads = snap.threads || {};
    var schedules = snap.schedules || {};
    var h = "";
    h += '<div class="cockpit-cards">';

    h += card(
      "Health",
      '<span class="' +
        (health.status === "healthy"
          ? "status-healthy"
          : "status-degraded") +
        '">' +
        esc(health.status || "unknown") +
        "</span>"
    );

    h += card("Version", esc(status.version || "unknown"));
    h += card("Uptime", formatDuration(status.uptime_seconds || 0));

    h += card(
      "Active Threads",
      '<span class="cockpit-card-value">' + (threads.active_count || 0) + "</span>"
    );

    h += card(
      "Schedules",
      (schedules.enabled || 0) +
        " / " +
        (schedules.total || 0) +
        " enabled"
    );

    if (snap.project) {
      h += card(
        "Project",
        '<span class="cockpit-card-value path">' +
          esc(snap.project.path) +
          "</span>"
      );
    }

    h += card(
      "GC",
      '<span class="' +
        (snap.gc.running ? "status-running" : "status-idle") +
        '">' +
        (snap.gc.running ? "● Running" : "○ Idle") +
        "</span>"
    );

    h += "</div>";
    return h;
  }

  // ── Items tab ────────────────────────────────────────────────────

  function renderItems() {
    var h = "";

    // Filter bar
    h += '<div style="display:flex;gap:0.5rem;margin-bottom:1rem;flex-wrap:wrap;align-items:center">';
    h += '<input type="search" class="cockpit-filter-input" data-filter="query" placeholder="SEARCH > kind:id" value="' + attrEsc(state.itemsFilter.query) + '" style="min-width:14rem">';
    h += '<select class="cockpit-filter-select" data-filter="kind">';
    h += '<option value="">ALL KINDS</option>';
    if (state.items && state.items.counts && state.items.counts.by_kind) {
      Object.keys(state.items.counts.by_kind).sort().forEach(function (k) {
        h += '<option value="' + attrEsc(k) + '"' + (state.itemsFilter.kind === k ? ' selected' : '') + '>' + esc(k) + ' (' + state.items.counts.by_kind[k] + ')</option>';
      });
    }
    h += '</select>';
    h += '<select class="cockpit-filter-select" data-filter="space">';
    h += '<option value="">ALL SPACES</option>';
    h += '<option value="system"' + (state.itemsFilter.space === "system" ? ' selected' : '') + '>system</option>';
    h += '<option value="user"' + (state.itemsFilter.space === "user" ? ' selected' : '') + '>user</option>';
    h += '<option value="project"' + (state.itemsFilter.space === "project" ? ' selected' : '') + '>project</option>';
    h += '</select>';
    h += '<button class="cockpit-filter-btn" data-action="reload">⟳ RELOAD</button>';
    h += "</div>";

    // Items table
    if (state.itemsError) {
      h += renderErrorState("Failed to load items", state.itemsError);
      return h;
    }

    if (state.itemsLoading) {
      h += '<div class="cockpit-empty">Loading items…</div>';
      return h;
    }

    if (!state.items) {
      h += '<div class="cockpit-empty">No items loaded</div>';
      return h;
    }

    var items = state.items.items || [];
    if (items.length === 0) {
      h += '<div class="cockpit-empty">No items match current filters</div>';
      return h;
    }

    // Summary
    var counts = state.items.counts || {};
    var byKind = counts.by_kind || {};
    var bySpace = counts.by_space || {};
    h += '<div style="margin-bottom:0.75rem;color:var(--fg4);font-size:0.72rem">';
    h += items.length + " items shown";
    var kindParts = Object.keys(byKind).sort().map(function (k) { return k + ":" + byKind[k]; });
    if (kindParts.length) h += " · " + kindParts.join(" ");
    var spaceParts = Object.keys(bySpace).sort().map(function (s) { return s + ":" + bySpace[s]; });
    if (spaceParts.length) h += " · " + spaceParts.join(" ");
    h += "</div>";

    // Table
    h += '<div class="cockpit-table-wrap"><table class="cockpit-table">';
    h += "<thead><tr>";
    h += "<th>Ref</th><th>Kind</th><th>Label</th><th>Space</th><th>Trust</th>";
    h += "</tr></thead><tbody>";
    items.forEach(function (item) {
      var trustHtml = "";
      if (item.trust) {
        var cls = "rye-badge-" + (item.trust.class || "unknown");
        trustHtml = '<span class="rye-badge ' + attrEsc(cls) + '">' + esc(item.trust.class) + "</span>";
      }
      h += '<tr class="cockpit-item-row" data-ref="' + attrEsc(item.canonical_ref) + '">';
      h += '<td><button class="rye-relation-link" data-inspect="' + attrEsc(item.canonical_ref) + '">' + esc(item.canonical_ref) + "</button></td>";
      h += "<td>" + esc(item.item_kind) + "</td>";
      h += "<td>" + esc(item.label) + "</td>";
      h += "<td>" + tag(item.space) + "</td>";
      h += "<td>" + trustHtml + "</td>";
      h += "</tr>";
    });
    h += "</tbody></table></div>";

    return h;
  }

  // ── Threads tab ──────────────────────────────────────────────────

  function renderThreads() {
    if (state.threadsError) {
      return renderErrorState("Failed to load threads", state.threadsError);
    }

    if (state.threadsLoading) {
      return '<div class="cockpit-empty">Loading threads…</div>';
    }

    if (!state.threads) {
      return '<div class="cockpit-empty">No threads loaded</div>';
    }

    var threads = state.threads.threads || [];
    if (threads.length === 0) {
      return '<div class="cockpit-empty">No threads found</div>';
    }

    var h = "";
    h += '<div style="margin-bottom:0.75rem;color:var(--fg4);font-size:0.72rem">';
    h += threads.length + " threads";
    h += ' <button class="cockpit-filter-btn" data-action="reload-threads" style="margin-left:0.75rem">⟳ RELOAD</button>';
    h += "</div>";

    h += '<div class="cockpit-table-wrap"><table class="cockpit-table">';
    h += "<thead><tr>";
    h += "<th>Thread</th><th>Kind</th><th>Ref</th><th>Status</th><th>Mode</th><th>Created</th><th>Updated</th>";
    h += "</tr></thead><tbody>";
    threads.forEach(function (t) {
      var statusCls = "";
      if (t.status === "completed" || t.status === "succeeded") statusCls = "status-healthy";
      else if (t.status === "running") statusCls = "status-running";
      else if (t.status === "failed" || t.status === "cancelled") statusCls = "status-degraded";

      h += "<tr>";
      h += "<td>" + esc((t.thread_id || "").slice(0, 12)) + "…</td>";
      h += "<td>" + esc(t.kind || "—") + "</td>";
      h += "<td>" + esc(t.item_ref || "—") + "</td>";
      h += '<td><span class="' + attrEsc(statusCls) + '">' + esc(t.status || "—") + "</span></td>";
      h += "<td>" + esc(t.launch_mode || "—") + "</td>";
      h += "<td>" + esc(t.created_at || "—") + "</td>";
      h += "<td>" + esc(t.updated_at || "—") + "</td>";
      h += "</tr>";
    });
    h += "</tbody></table></div>";

    return h;
  }

  // ── Schedules tab ────────────────────────────────────────────────

  function renderSchedules() {
    if (state.schedulesError) {
      return renderErrorState("Failed to load schedules", state.schedulesError);
    }

    if (state.schedulesLoading) {
      return '<div class="cockpit-empty">Loading schedules…</div>';
    }

    if (!state.schedules) {
      return '<div class="cockpit-empty">No schedules loaded</div>';
    }

    var schedules = state.schedules.schedules || [];
    if (schedules.length === 0) {
      return '<div class="cockpit-empty">No schedules found</div>';
    }

    var h = "";
    h += '<div style="margin-bottom:0.75rem;color:var(--fg4);font-size:0.72rem">';
    h += schedules.length + " schedules";
    h += ' <button class="cockpit-filter-btn" data-action="reload-schedules" style="margin-left:0.75rem">⟳ RELOAD</button>';
    h += "</div>";

    h += '<div class="cockpit-table-wrap"><table class="cockpit-table">';
    h += "<thead><tr>";
    h += "<th>Ref</th><th>Type</th><th>Expression</th><th>TZ</th><th>Enabled</th><th>Last Fire</th><th>Last Status</th><th>Fires</th>";
    h += "</tr></thead><tbody>";
    schedules.forEach(function (s) {
      var enabledCls = s.enabled ? "status-healthy" : "status-idle";
      var lastStatusCls = "";
      if (s.last_fire_status === "succeeded") lastStatusCls = "status-healthy";
      else if (s.last_fire_status === "failed") lastStatusCls = "status-degraded";

      h += "<tr>";
      h += "<td>" + esc(s.item_ref || "—") + "</td>";
      h += "<td>" + esc(s.schedule_type || "—") + "</td>";
      h += "<td style='font-size:0.7rem'>" + esc(s.expression || "—") + "</td>";
      h += "<td>" + esc(s.timezone || "—") + "</td>";
      h += '<td><span class="' + attrEsc(enabledCls) + '">' + (s.enabled ? "ON" : "OFF") + "</span></td>";
      h += "<td>" + esc(s.last_fire_at || "—") + "</td>";
      h += '<td><span class="' + attrEsc(lastStatusCls) + '">' + esc(s.last_fire_status || "—") + "</span></td>";
      h += "<td>" + (s.total_fires || 0) + "</td>";
      h += "</tr>";
    });
    h += "</tbody></table></div>";

    return h;
  }

  // ── GC tab ───────────────────────────────────────────────────────

  function renderGc() {
    if (state.gcError) {
      return renderErrorState("Failed to load GC status", state.gcError);
    }

    if (state.gcLoading) {
      return '<div class="cockpit-empty">Loading GC status…</div>';
    }

    if (!state.gcStatus) {
      return '<div class="cockpit-empty">No GC data loaded</div>';
    }

    var gc = state.gcStatus;
    var h = "";

    // Status card
    h += '<div class="cockpit-cards" style="margin-bottom:1rem">';
    h += card("Status",
      '<span class="' + (gc.running ? "status-running" : "status-idle") + '" style="font-size:1.1rem">' +
      (gc.running ? "● RUNNING" : "○ Idle") +
      "</span>"
    );
    h += card("Recent Events", (gc.recent_events || []).length + " recorded");
    h += "</div>";

    // Running state
    if (gc.running && gc.state) {
      h += section("Current Run", function () {
        return '<pre class="cockpit-source-view">' + esc(JSON.stringify(gc.state, null, 2)) + "</pre>";
      });
    }

    // Event history
    var events = gc.recent_events || [];
    if (events.length > 0) {
      h += section("Event History", function () {
        return table(
          ["Timestamp", "Dry Run", "Compact", "Objects", "Blobs", "Deleted", "Freed", "Duration"],
          events.map(function (e) {
            return [
              esc(e.timestamp || ""),
              e.dry_run ? "YES" : "no",
              e.compact ? "YES" : "no",
              String(e.reachable_objects || 0),
              String(e.reachable_blobs || 0),
              String(e.deleted_objects || 0) + " obj / " + String(e.deleted_blobs || 0) + " blob",
              formatBytes(e.freed_bytes || 0),
              (e.duration_ms || 0) + "ms",
            ];
          })
        );
      });
    } else {
      h += section("Event History", function () {
        return '<div class="cockpit-empty">No GC events recorded</div>';
      });
    }

    // Refresh button
    h += '<div style="margin-top:1rem">';
    h += '<button class="cockpit-filter-btn" data-action="reload-gc">⟳ REFRESH</button>';
    h += "</div>";

    return h;
  }

  // ── Remotes tab ────────────────────────────────────────────────

  function renderRemotes() {
    if (state.remotesError) {
      return renderErrorState("Failed to load remotes", state.remotesError);
    }

    if (state.remotesLoading) {
      return '<div class="cockpit-empty">Loading remotes…</div>';
    }

    if (!state.remotes) {
      return '<div class="cockpit-empty">No remotes loaded</div>';
    }

    var remotes = state.remotes.remotes || [];
    var h = "";

    h += '<div style="margin-bottom:0.75rem;color:var(--fg4);font-size:0.72rem">';
    h += remotes.length + " remotes configured";
    h += ' <button class="cockpit-filter-btn" data-action="reload-remotes" style="margin-left:0.75rem">⟳ RELOAD</button>';
    h += "</div>";

    if (state.remotesProbeError) {
      h += renderErrorState("Failed to probe " + (state.remotesProbeName || "remote"), state.remotesProbeError);
    }

    if (remotes.length === 0) {
      h += '<div class="cockpit-empty">No remotes configured</div>';
      return h;
    }

    // Probe result display
    if (state.remotesProbeResult) {
      var probe = state.remotesProbeResult;
      var probeHealth = probe.health || {};
      var statusCls = (probeHealth.status === "healthy" || probeHealth.status === "ok")
        ? "status-healthy" : "status-degraded";
      h += '<div class="cockpit-card" style="margin-bottom:1rem;border-color:var(--fg4)">';
      h += '<div class="cockpit-card-label">Last Probe — ' + esc(remoteProbeName(probe)) + '</div>';
      h += '<div style="display:flex;gap:1rem;align-items:center">';
      h += '<span class="' + attrEsc(statusCls) + '" style="font-size:0.9rem">● ' + esc(probeHealth.status || "unknown") + '</span>';
      if (probeHealth.version) h += '<span style="color:var(--fg4)">v' + esc(probeHealth.version) + '</span>';
      h += "</div></div>";
    }

    h += '<div class="cockpit-table-wrap"><table class="cockpit-table">';
    h += "<thead><tr>";
    h += "<th>Name</th><th>URL</th><th>Principal</th><th>Actions</th>";
    h += "</tr></thead><tbody>";
    remotes.forEach(function (r) {
      h += "<tr>";
      h += "<td><strong>" + esc(r.name) + "</strong></td>";
      h += "<td style='font-size:0.72rem'>" + esc(r.url) + "</td>";
      h += "<td>" + esc(r.principal_id.slice(0, 16)) + "…</td>";
      h += '<td><button class="cockpit-filter-btn" data-probe="' + attrEsc(r.name) + '">PROBE</button></td>';
      h += "</tr>";

      // Project bindings
      var bindings = r.project_bindings;
      if (bindings && Object.keys(bindings).length > 0) {
        h += '<tr><td colspan="4" style="padding:0 0.65rem 0.15rem">';
        h += '<span style="color:var(--fg4);font-size:0.65rem;letter-spacing:0.06em">PROJECT BINDINGS</span>';
        h += "</td></tr>";
        Object.keys(bindings).forEach(function (key) {
          var b = bindings[key];
          h += '<tr><td colspan="4" style="padding:0.15rem 0.65rem">';
          h += '<span style="color:var(--fg4)">local:</span> ' + esc(key);
          h += ' → <span style="color:var(--orange)">remote:</span> ' + esc(b.remote_project_path || "—");
          h += "</td></tr>";
        });
      }
    });
    h += "</tbody></table></div>";

    return h;
  }

  // ── Files tab ────────────────────────────────────────────────────

  function renderFiles() {
    if (state.filesError) {
      return renderFileControls() + renderErrorState("Failed to load files", state.filesError);
    }

    if (state.filesLoading) {
      return '<div class="cockpit-empty">Loading files…</div>';
    }

    var h = renderFileControls();

    if (state.readFileError) {
      h += renderErrorState("Failed to read file", state.readFileError);
    }

    // File content viewer (when a file is being read)
    if (state.readFileResult) {
      var fr = state.readFileResult;
      h += section("File: " + esc(fr.path), function () {
        var h2 = "";
        h2 += '<div style="margin-bottom:0.35rem;color:var(--fg4);font-size:0.68rem">';
        h2 += (fr.size || 0) + " bytes";
        if (fr.truncated) h2 += " (truncated at 256 KB)";
        h2 += "</div>";
        h2 += '<pre class="cockpit-source-view">' + esc(fr.content) + "</pre>";
        h2 += '<div style="margin-top:0.35rem">';
        h2 += '<button class="cockpit-filter-btn" data-action="close-file">← BACK</button>';
        h2 += "</div>";
        return h2;
      });
      return h;
    }

    // File listing
    if (!state.files) {
      return '<div class="cockpit-empty">No files loaded</div>';
    }

    // Breadcrumb
    h += '<div style="margin-bottom:0.75rem;color:var(--fg4);font-size:0.72rem;display:flex;gap:0.35rem;align-items:center">';
    h += '<button class="rye-relation-link" data-navigate-absolute="" style="font-size:0.72rem;color:var(--fg2)">' + esc(state.filesRoot) + "</button>";
    if (state.filesPath) {
      state.filesPath.split("/").filter(Boolean).forEach(function (seg, i, parts) {
        h += ' <span style="color:var(--fg4)">›</span> ';
        var crumbPath = parts.slice(0, i + 1).join("/");
        h += '<button class="rye-relation-link" data-navigate-absolute="' + attrEsc(crumbPath) + '" style="font-size:0.72rem">' + esc(seg) + "</button>";
      });
    }
    h += "</div>";

    var entries = state.files.entries || [];
    if (entries.length === 0) {
      h += '<div class="cockpit-empty">Empty directory</div>';
      return h;
    }

    // Directories section
    var dirs = entries.filter(function (e) { return e.is_dir; });
    var files = entries.filter(function (e) { return !e.is_dir; });

    if (dirs.length > 0) {
      h += section("Directories", function () {
        var h2 = "";
        dirs.forEach(function (d) {
          h2 += '<button class="cockpit-file-entry" data-navigate-relative="' + attrEsc(d.name) + '">';
          h2 += '<span style="color:var(--orange);font-size:0.72rem">📁</span> ';
          h2 += esc(d.name);
          h2 += "</button>";
        });
        return h2;
      });
    }

    if (files.length > 0) {
      h += section("Files", function () {
        var h2 = "";
        files.forEach(function (f) {
          h2 += '<button class="cockpit-file-entry" data-readfile="' + attrEsc(f.name) + '">';
          h2 += '<span style="color:var(--fg4);font-size:0.72rem">📄</span> ';
          h2 += esc(f.name);
          h2 += '<span style="color:var(--fg4);font-size:0.6rem;margin-left:0.5rem">';
          h2 += formatBytes(f.size || 0);
          h2 += "</span>";
          h2 += "</button>";
        });
        return h2;
      });
    }

    return h;
  }

  // ── Topology tab ──────────────────────────────────────────────────

  function renderTopology() {
    if (state.topologyError) {
      return renderErrorState("Failed to load topology", state.topologyError);
    }

    if (state.topologyLoading || !state.topology) {
      return (
        '<div class="cockpit-empty">Loading topology…</div>' +
        '<div class="cockpit-topology-container" id="cockpit-topology-graph"></div>'
      );
    }

    var meta = state.topology.metadata || {};
    var h = "";
    h += '<div style="margin-bottom:0.5rem;color:var(--fg4);font-size:0.72rem">';
    h += (state.topology.nodes || []).length + " nodes · ";
    h += (state.topology.edges || []).length + " edges";
    if (meta.project_root) h += " · " + esc(meta.project_root);
    h += "</div>";
    h += '<div class="cockpit-topology-container" id="cockpit-topology-graph"></div>';
    return h;
  }

  // ── Inspector ─────────────────────────────────────────────────────

  function renderInspector() {
    var h = "";
    h += '<div class="cockpit-inspector-header">Inspector</div>';
    h += '<div class="cockpit-inspector-body">';

    if (state.inspectLoading) {
      h += '<div class="cockpit-empty">Loading…</div>';
    } else if (state.inspectError) {
      h += renderErrorState("Failed to inspect item", state.inspectError);
    } else if (state.inspectedItem) {
      h += renderInspectedItem();
    } else if (state.snapshot) {
      h += renderInspectorContent();
    } else {
      h += '<div class="cockpit-empty">Waiting for snapshot…</div>';
    }

    h += "</div>";
    return h;
  }

  function renderInspectorContent() {
    var snap = state.snapshot;
    var h = "";

    h += '<div class="inspector-prompt">&gt; inspect session</div>';

    h += '<dl class="inspector-kv">';
    h += kv("Session", snap.session.session_id.slice(0, 12) + "…");
    h += kv("Surface", esc(snap.session.surface_ref));
    if (snap.project) h += kv("Project", esc(snap.project.path));
    h += kv("Read Only", snap.session.read_only ? "YES" : "NO");
    h += kv("Identity", esc(snap.local_node.identity.principal_id));
    h += kv("Fingerprint", esc(snap.local_node.identity.fingerprint.slice(0, 16) + "…"));

    if (snap.session.granted_caps && snap.session.granted_caps.length > 0) {
      h += "</dl>";
      h += '<div class="inspector-section">';
      h += '<div class="inspector-section-title">Granted Capabilities</div>';
      h += '<div style="margin-top:0.35rem">';
      snap.session.granted_caps.forEach(function (cap) {
        h += '<div class="cockpit-tag" style="margin:0.15rem 0.15rem">' + esc(cap) + "</div> ";
      });
      h += "</div>";
      h += "</div>";
    } else {
      h += kv("Caps", "none");
      h += "</dl>";
    }

    return h;
  }

  function renderInspectedItem() {
    var data = state.inspectedItem;
    var item = data.item;
    var h = "";

    h += '<div class="inspector-prompt">&gt; inspect ' + esc(item.canonical_ref) + '</div>';

    h += '<dl class="inspector-kv">';
    h += kv("Kind", esc(item.item_kind));
    h += kv("Bare ID", esc(item.bare_id));
    h += kv("Space", tag(item.space));
    h += kv("Executable", item.executable ? "YES" : "NO");

    if (item.trust) {
      h += kv("Trust", esc(item.trust.class));
      if (item.trust.signer) h += kv("Signer", esc(item.trust.signer));
    }

    h += "</dl>";

    // Shadowed
    if (item.shadowed && item.shadowed.length > 0) {
      h += '<div class="inspector-section">';
      h += '<div class="inspector-section-title">Shadowed By</div>';
      item.shadowed.forEach(function (s) {
        h += '<div style="color:var(--fg4);font-size:0.72rem;margin:0.15rem 0">';
        h += esc(s.space) + " — " + esc(s.label);
        h += "</div>";
      });
      h += "</div>";
    }

    // Raw content
    if (data.raw) {
      h += '<div class="inspector-section">';
      h += '<div class="inspector-section-title">Raw Source';
      h += " <span style='color:var(--fg4);font-weight:400'>" + formatBytes(data.raw.bytes);
      if (data.raw.truncated) h += " (truncated)";
      h += "</span></div>";
      h += '<pre class="cockpit-source-view">' + esc(data.raw.content) + "</pre>";
      h += "</div>";
    }

    // Effective content
    if (data.effective) {
      h += '<div class="inspector-section">';
      h += '<div class="inspector-section-title">Effective</div>';
      h += '<pre class="cockpit-source-view">' + esc(JSON.stringify(data.effective, null, 2)) + "</pre>";
      h += "</div>";
    }

    // Back button
    h += '<div style="margin-top:1rem">';
    h += '<button class="cockpit-filter-btn" data-action="clear-inspect">← BACK TO SESSION</button>';
    h += "</div>";

    return h;
  }

  // ── Event binding ────────────────────────────────────────────────

  function bindEvents() {
    var tabs = document.querySelectorAll(".cockpit-nav-item[data-tab]");
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].addEventListener("click", function () {
        switchTab(this.getAttribute("data-tab"));
      });
    }

    // Items filter inputs
    var filters = document.querySelectorAll("[data-filter]");
    for (var j = 0; j < filters.length; j++) {
      var el = filters[j];
      var evtType = el.tagName === "SELECT" ? "change" : "input";
      el.addEventListener(evtType, function () {
        var key = this.getAttribute("data-filter");
        state.itemsFilter[key] = this.value;
        state.items = null; // Force reload
      });
      // Also listen for Enter on search
      if (el.type === "search") {
        el.addEventListener("keydown", function (e) {
          if (e.key === "Enter") {
            state.itemsFilter.query = this.value;
            state.items = null;
            loadItems();
          }
        });
      }
    }

    // Reload button
    var reloadBtn = document.querySelector("[data-action='reload']");
    if (reloadBtn) {
      reloadBtn.addEventListener("click", function () {
        state.items = null;
        loadItems();
      });
    }

    // Reload threads
    var reloadThreadsBtn = document.querySelector("[data-action='reload-threads']");
    if (reloadThreadsBtn) {
      reloadThreadsBtn.addEventListener("click", function () {
        state.threads = null;
        loadThreads();
      });
    }

    // Reload schedules
    var reloadSchedulesBtn = document.querySelector("[data-action='reload-schedules']");
    if (reloadSchedulesBtn) {
      reloadSchedulesBtn.addEventListener("click", function () {
        state.schedules = null;
        loadSchedules();
      });
    }

    // Reload GC
    var reloadGcBtn = document.querySelector("[data-action='reload-gc']");
    if (reloadGcBtn) {
      reloadGcBtn.addEventListener("click", function () {
        state.gcStatus = null;
        loadGcStatus();
      });
    }

    // Reload remotes
    var reloadRemotesBtn = document.querySelector("[data-action='reload-remotes']");
    if (reloadRemotesBtn) {
      reloadRemotesBtn.addEventListener("click", function () {
        state.remotes = null;
        state.remotesProbeResult = null;
        loadRemotes();
      });
    }

    // Remote probe buttons
    var probeBtns = document.querySelectorAll("[data-probe]");
    for (var p = 0; p < probeBtns.length; p++) {
      probeBtns[p].addEventListener("click", function () {
        probeRemote(this.getAttribute("data-probe"));
      });
    }

    // File root buttons
    var rootBtns = document.querySelectorAll("[data-set-root]");
    for (var r = 0; r < rootBtns.length; r++) {
      rootBtns[r].addEventListener("click", function () {
        state.filesRoot = this.getAttribute("data-set-root");
        state.filesPath = "";
        state.files = null;
        state.readFileResult = null;
        loadFiles(state.filesRoot, "");
      });
    }

    // File navigation (breadcrumb clicks use absolute paths; directory clicks are relative)
    var absoluteNavBtns = document.querySelectorAll("[data-navigate-absolute]");
    for (var n = 0; n < absoluteNavBtns.length; n++) {
      absoluteNavBtns[n].addEventListener("click", function () {
        state.filesPath = this.getAttribute("data-navigate-absolute") || "";
        state.files = null;
        state.readFileResult = null;
        loadFiles(state.filesRoot, state.filesPath);
      });
    }

    var relativeNavBtns = document.querySelectorAll("[data-navigate-relative]");
    for (var rn = 0; rn < relativeNavBtns.length; rn++) {
      relativeNavBtns[rn].addEventListener("click", function () {
        var segment = this.getAttribute("data-navigate-relative");
        state.filesPath = state.filesPath ? state.filesPath + "/" + segment : segment;
        state.files = null;
        state.readFileResult = null;
        loadFiles(state.filesRoot, state.filesPath);
      });
    }

    // File entry clicks (files)
    var fileEntries = document.querySelectorAll("[data-readfile]");
    for (var f = 0; f < fileEntries.length; f++) {
      fileEntries[f].addEventListener("click", function () {
        readFile(state.filesRoot, state.filesPath ? state.filesPath + "/" + this.getAttribute("data-readfile") : this.getAttribute("data-readfile"));
      });
    }

    // Close file viewer button
    var closeFileBtn = document.querySelector("[data-action='close-file']");
    if (closeFileBtn) {
      closeFileBtn.addEventListener("click", function () {
        state.readFileResult = null;
        render();
      });
    }

    // Clear inspect button
    var clearBtn = document.querySelector("[data-action='clear-inspect']");
    if (clearBtn) {
      clearBtn.addEventListener("click", function () {
        state.inspectedItem = null;
        render();
      });
    }

    // Item inspect clicks
    var inspectBtns = document.querySelectorAll("[data-inspect]");
    for (var k = 0; k < inspectBtns.length; k++) {
      inspectBtns[k].addEventListener("click", function (e) {
        e.preventDefault();
        e.stopPropagation();
        inspectItem(this.getAttribute("data-inspect"));
      });
    }
  }

  function switchTab(tab) {
    state.activeTab = tab;
    // Clear inspector when switching away from items
    if (tab !== "items") {
      // Keep inspector state, just re-render
    }
    render();
  }

  // ── Helpers ───────────────────────────────────────────────────────

  function renderErrorState(title, message) {
    return (
      '<div class="cockpit-empty" style="color:var(--yellow)">' +
      esc(title) +
      (message ? ": " + esc(message) : "") +
      "</div>"
    );
  }

  function renderFileControls() {
    var h = "";
    h += '<div style="display:flex;gap:0.5rem;margin-bottom:1rem;align-items:center;flex-wrap:wrap">';
    h += '<span style="color:var(--fg4);font-size:0.72rem;text-transform:uppercase">ROOT:</span>';
    h += '<button class="cockpit-filter-btn' + (state.filesRoot === "project" ? ' active' : '') + '" data-set-root="project">Project</button>';
    h += '<button class="cockpit-filter-btn' + (state.filesRoot === "project_ai" ? ' active' : '') + '" data-set-root="project_ai">.ai</button>';
    h += "</div>";
    return h;
  }

  function remoteProbeName(probe) {
    if (state.remotesProbeName) return state.remotesProbeName;
    if (!probe || !probe.remote) return "—";
    if (typeof probe.remote === "string") return probe.remote;
    return probe.remote.name || probe.remote.id || probe.remote.url || "—";
  }

  function section(title, renderFn) {
    return (
      '<div class="cockpit-section">' +
      '<div class="cockpit-section-title">' + esc(title) + "</div>" +
      renderFn() +
      "</div>"
    );
  }

  function card(label, valueHtml) {
    return (
      '<div class="cockpit-card">' +
      '<div class="cockpit-card-label">' + esc(label) + "</div>" +
      '<div class="cockpit-card-value">' + valueHtml + "</div>" +
      "</div>"
    );
  }

  function tag(space) {
    return '<span class="cockpit-tag cockpit-tag-' + attrEsc(space) + '">' + esc(space) + "</span>";
  }

  function table(headers, rows) {
    var h = '<div class="cockpit-table-wrap"><table class="cockpit-table">';
    h += "<thead><tr>";
    headers.forEach(function (hdr) {
      h += "<th>" + esc(hdr) + "</th>";
    });
    h += "</tr></thead><tbody>";
    rows.forEach(function (row) {
      h += "<tr>";
      row.forEach(function (cell) {
        h += "<td>" + cell + "</td>";
      });
      h += "</tr>";
    });
    h += "</tbody></table></div>";
    return h;
  }

  function kv(key, val) {
    return "<dt>" + esc(key) + "</dt><dd>" + val + "</dd>";
  }

  function esc(s) {
    var d = document.createElement("div");
    d.appendChild(document.createTextNode(s || ""));
    return d.innerHTML;
  }

  function attrEsc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;");
  }

  function formatDuration(secs) {
    if (secs < 60) return secs + "s";
    if (secs < 3600) return Math.floor(secs / 60) + "m " + (secs % 60) + "s";
    var h = Math.floor(secs / 3600);
    var m = Math.floor((secs % 3600) / 60);
    return h + "h " + m + "m";
  }

  function formatBytes(b) {
    if (b === 0) return "0 B";
    var units = ["B", "KB", "MB", "GB"];
    var i = 0;
    var n = b;
    while (n >= 1024 && i < units.length - 1) {
      n /= 1024;
      i++;
    }
    return n.toFixed(1) + " " + units[i];
  }

  // ── Export ────────────────────────────────────────────────────────

  window.RyeCockpit = { boot: boot };
})();
