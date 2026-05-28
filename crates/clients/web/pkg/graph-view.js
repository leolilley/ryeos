// RyeOS topology view — thin 3d-force-graph adapter.
//
// Rust owns topology semantics. This browser layer owns persistent UI state,
// filtering, camera focus, and the item/relation inspector.

(function () {
  "use strict";

  var COLORS = {
    directive: 0xfe8019,
    graph: 0xd3869b,
    graph_node: 0xd3869b,
    knowledge: 0x8ec07c,
    tool: 0xfabd2f,
    surface: 0xd3869b,
    client: 0x83a598,
    parser: 0x83a598,
    handler: 0xa89984,
    runtime: 0xfb4934,
    kind_schema: 0xebdbb2,
    surface_view: 0xd3869b,
  };

  var COLOR_CSS = {
    directive: "#fe8019",
    graph: "#d3869b",
    graph_node: "#d3869b",
    knowledge: "#8ec07c",
    tool: "#fabd2f",
    surface: "#d3869b",
    client: "#83a598",
    parser: "#83a598",
    handler: "#a89984",
    runtime: "#fb4934",
    kind_schema: "#ebdbb2",
    surface_view: "#d3869b",
  };

  var FALLBACK_COLOR = 0xa89984;
  var ORPHAN_COLOR = 0x504945;
  var SELECTED_COLOR = 0xfe8019;

  var instances = typeof WeakMap !== "undefined" ? new WeakMap() : null;

  function esc(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function uniq(values) {
    var seen = {};
    return values
      .filter(function (value) {
        if (!value || seen[value]) return false;
        seen[value] = true;
        return true;
      })
      .sort();
  }

  function optionList(values, selected) {
    return values
      .map(function (value) {
        return (
          '<option value="' + esc(value) + '"' +
          (value === selected ? " selected" : "") + ">" + esc(value) + "</option>"
        );
      })
      .join("");
  }

  function computeDegrees(items, relations) {
    var degrees = {};
    items.forEach(function (item) {
      degrees[item.id] = { in: 0, out: 0, total: 0 };
    });
    relations.forEach(function (rel) {
      if (degrees[rel.from]) {
        degrees[rel.from].out++;
        degrees[rel.from].total++;
      }
      if (degrees[rel.to]) {
        degrees[rel.to].in++;
        degrees[rel.to].total++;
      }
    });
    return degrees;
  }

  function relationList(relations, itemId, direction) {
    return relations.filter(function (rel) {
      return direction === "out" ? rel.from === itemId : rel.to === itemId;
    });
  }

  function neighborhood(selectedId, relations, mode) {
    if (!selectedId || mode === "system") return null;
    var visited = {};
    var frontier = [selectedId];
    visited[selectedId] = true;
    var depth = mode === "local2" ? 2 : 1;

    for (var hop = 0; hop < depth; hop++) {
      var next = [];
      relations.forEach(function (rel) {
        var candidates = [];
        if (mode === "downstream") {
          if (visited[rel.from]) candidates.push(rel.to);
        } else if (mode === "upstream") {
          if (visited[rel.to]) candidates.push(rel.from);
        } else {
          if (visited[rel.from]) candidates.push(rel.to);
          if (visited[rel.to]) candidates.push(rel.from);
        }
        candidates.forEach(function (id) {
          if (!visited[id]) {
            visited[id] = true;
            next.push(id);
          }
        });
      });
      frontier = next;
      if (!frontier.length) break;
    }
    return visited;
  }

  function itemColor(item, degrees) {
    if (item.missing) return 0xfb4934;
    if (degrees && degrees[item.id] && degrees[item.id].total === 0) return ORPHAN_COLOR;
    return COLORS[item.kind] || FALLBACK_COLOR;
  }

  function nodeId(value) {
    if (!value) return null;
    return typeof value === "object" ? value.id : value;
  }

  function chooseInitialItem(topology, itemById, degrees) {
    var candidates = [
      topology.metadata && topology.metadata.root_surface,
      "surface:ryeos/cockpit/graph",
      "client:ryeos/web",
    ];
    for (var i = 0; i < candidates.length; i++) {
      if (candidates[i] && itemById[candidates[i]]) return candidates[i];
    }
    var best = null;
    Object.keys(itemById).forEach(function (id) {
      var item = itemById[id];
      if (item.virtual || item.missing) return;
      if (!best || (degrees[id] && degrees[id].total) > (degrees[best] && degrees[best].total)) {
        best = id;
      }
    });
    return best || Object.keys(itemById)[0] || null;
  }

  function TopologyView(app, topology, context) {
    this.app = app;
    this.topology = topology;
    this.context = context || {};
    this.items = [];
    this.relations = [];
    this.itemById = {};
    this.degrees = {};
    this.graph = null;
    this.state = {
      selectedItemId: null,
      query: "",
      kind: "all",
      relationType: "all",
      space: "all",
      focusMode: "local1",
      layoutMode: "force",
      hexGrid: false,
      searchMatches: [],
      searchIndex: -1,
    };
    this.mount();
    this.setTopology(topology, true);
  }

  TopologyView.prototype.mount = function () {
    this.app.innerHTML =
      '<div class="rye-graph-shell">' +
      '<section class="rye-graph-main">' +
      '<div class="rye-graph-header">' +
      '<div><div class="rye-graph-title">RYE OS · TOPOLOGY INSPECTOR</div>' +
      '<div class="rye-graph-meta" id="rye-topology-meta">loading topology</div></div>' +
      '<div class="rye-graph-controls">' +
      '<input id="rye-graph-search" type="search" placeholder="SEARCH > item ref" />' +
      '<select id="rye-graph-space"><option value="all">all spaces</option><option value="project">project</option><option value="user">user</option><option value="system">system</option></select>' +
      '<select id="rye-graph-kind"></select>' +
      '<select id="rye-graph-relation"></select>' +
      '<select id="rye-graph-focus"><option value="system">system</option><option value="local1">local 1</option><option value="local2">local 2</option><option value="upstream">upstream</option><option value="downstream">downstream</option></select>' +
      '<select id="rye-graph-layout"><option value="force">free</option><option value="layers">kind layers</option></select>' +
      '<label class="rye-toggle"><input id="rye-graph-hex" type="checkbox" /> hex</label>' +
      '</div></div>' +
      '<div class="rye-graph-canvas" id="rye-graph-canvas"></div>' +
      '<div class="rye-legend-container" id="rye-graph-legend"></div>' +
      '</section>' +
      '<aside class="rye-graph-details" id="rye-graph-details"></aside>' +
      '</div>';

    this.meta = this.app.querySelector("#rye-topology-meta");
    this.canvas = this.app.querySelector("#rye-graph-canvas");
    this.legend = this.app.querySelector("#rye-graph-legend");
    this.details = this.app.querySelector("#rye-graph-details");
    this.search = this.app.querySelector("#rye-graph-search");
    this.space = this.app.querySelector("#rye-graph-space");
    this.kind = this.app.querySelector("#rye-graph-kind");
    this.relationType = this.app.querySelector("#rye-graph-relation");
    this.focus = this.app.querySelector("#rye-graph-focus");
    this.layout = this.app.querySelector("#rye-graph-layout");
    this.hexGrid = this.app.querySelector("#rye-graph-hex");

    this.bindControls();
    this.graph = ForceGraph3D()(this.canvas)
      .backgroundColor("#1d2021")
      .nodeLabel(this.nodeLabel.bind(this))
      .nodeColor(this.renderNodeColor.bind(this))
      .nodeVal(function (item) { return item.kind === "kind_schema" ? 12 : item.virtual ? 4 : 7; })
      .nodeRelSize(3)
      .linkSource("source")
      .linkTarget("target")
      .linkColor(this.renderRelationColor.bind(this))
      .linkWidth(this.renderRelationWidth.bind(this))
      .linkDirectionalArrowLength(3.5)
      .linkDirectionalArrowRelPos(1)
      .linkDirectionalArrowColor(function () { return "rgba(250,189,47,0.72)"; })
      .onNodeClick(this.selectItem.bind(this))
      .onBackgroundClick(function () {})
      .onLinkClick(this.selectRelation.bind(this));
  };

  TopologyView.prototype.bindControls = function () {
    var self = this;
    this.search.addEventListener("input", function () {
      self.state.query = self.search.value.trim();
      self.updateSearchMatches();
      self.apply();
    });
    this.search.addEventListener("keydown", function (event) {
      if (event.key === "Enter") {
        event.preventDefault();
        self.jumpToSearchMatch(event.shiftKey ? -1 : 1);
      }
    });
    this.space.addEventListener("change", function () {
      self.state.space = self.space.value;
      self.apply();
    });
    this.kind.addEventListener("change", function () {
      self.state.kind = self.kind.value;
      self.apply();
    });
    this.relationType.addEventListener("change", function () {
      self.state.relationType = self.relationType.value;
      self.apply();
    });
    this.focus.addEventListener("change", function () {
      self.state.focusMode = self.focus.value;
      self.apply();
    });
    this.layout.addEventListener("change", function () {
      self.state.layoutMode = self.layout.value;
      self.apply();
      if (self.graph && typeof self.graph.d3ReheatSimulation === "function") {
        self.graph.d3ReheatSimulation();
      }
    });
    this.hexGrid.addEventListener("change", function () {
      self.state.hexGrid = self.hexGrid.checked;
      self.canvas.classList.toggle("hex-grid", self.state.hexGrid);
    });
    document.addEventListener("keydown", function (event) {
      if (event.target && /input|select|textarea/i.test(event.target.tagName)) return;
      if (event.key === "/") {
        event.preventDefault();
        self.search.focus();
      } else if (event.key === "n") {
        self.jumpToSearchMatch(event.shiftKey ? -1 : 1);
      } else if (event.key === "f") {
        self.graph.zoomToFit(600, 80);
      }
    });
  };

  TopologyView.prototype.setTopology = function (topology, initial) {
    this.topology = topology;
    this.items = (topology.nodes || []).map(function (n) { return Object.assign({}, n); });
    this.relations = topology.edges || [];
    this.itemById = {};
    this.items.forEach(function (item) { this.itemById[item.id] = item; }, this);
    this.degrees = computeDegrees(this.items, this.relations);
    this.populateControls();
    if (initial || !this.state.selectedItemId || !this.itemById[this.state.selectedItemId]) {
      this.state.selectedItemId = chooseInitialItem(topology, this.itemById, this.degrees);
    }
    this.updateSearchMatches();
    this.apply();
    this.renderDetails();
  };

  TopologyView.prototype.updateContext = function (context) {
    this.context = context || {};
    this.renderMeta(this.visibleItems || this.items, this.visibleRelations || this.relations);
  };

  TopologyView.prototype.populateControls = function () {
    var kinds = uniq(this.items.map(function (item) { return item.kind; }));
    var rels = uniq(this.relations.map(function (rel) { return rel.type; }));
    this.kind.innerHTML = '<option value="all">all items</option>' + optionList(kinds, this.state.kind);
    this.relationType.innerHTML = '<option value="all">all relations</option>' + optionList(rels, this.state.relationType);
    this.focus.value = this.state.focusMode;
    this.space.value = this.state.space;
    this.layout.value = this.state.layoutMode;
    this.hexGrid.checked = this.state.hexGrid;
    this.canvas.classList.toggle("hex-grid", this.state.hexGrid);
  };

  TopologyView.prototype.matchesSearch = function (item) {
    if (!this.state.query) return false;
    var q = this.state.query.toLowerCase();
    return [item.id, item.ref, item.label, item.kind, item.namespace, item.path]
      .join(" ")
      .toLowerCase()
      .indexOf(q) !== -1;
  };

  TopologyView.prototype.updateSearchMatches = function () {
    var self = this;
    this.state.searchMatches = this.state.query
      ? this.items.filter(function (item) { return self.matchesSearch(item); }).map(function (item) { return item.id; })
      : [];
    this.state.searchIndex = -1;
  };

  TopologyView.prototype.jumpToSearchMatch = function (direction) {
    var matches = this.state.searchMatches;
    if (!matches.length) return;
    if (this.state.searchIndex < 0) {
      this.state.searchIndex = direction < 0 ? 0 : -1;
    }
    this.state.searchIndex = (this.state.searchIndex + direction + matches.length) % matches.length;
    this.selectItem(this.itemById[matches[this.state.searchIndex]]);
  };

  TopologyView.prototype.filteredData = function () {
    var selectedId = this.state.selectedItemId;
    var nh = neighborhood(selectedId, this.relations, this.state.focusMode);
    var visibleIds = {};
    var self = this;
    var filteredItems = this.items.filter(function (item) {
      var include =
        (self.state.kind === "all" || item.kind === self.state.kind) &&
        (self.state.space === "all" || item.space === self.state.space) &&
        (!nh || nh[item.id]);
      if (include) visibleIds[item.id] = true;
      return include;
    });
    var filteredRelations = this.relations.filter(function (rel) {
      return visibleIds[rel.from] && visibleIds[rel.to] &&
        (self.state.relationType === "all" || rel.type === self.state.relationType);
    });
    return {
      nodes: this.applyLayoutMode(filteredItems),
      links: filteredRelations.map(function (rel) { return { source: rel.from, target: rel.to, relation: rel }; }),
      items: filteredItems,
      relations: filteredRelations,
    };
  };

  TopologyView.prototype.applyLayoutMode = function (items) {
    if (this.state.layoutMode !== "layers") {
      items.forEach(function (item) {
        delete item.fz;
        delete item.__layerIndex;
        delete item.__layerCount;
      });
      return items;
    }

    var kinds = uniq(this.items.map(function (item) { return item.kind; }));
    var layerByKind = {};
    kinds.forEach(function (kind, index) {
      layerByKind[kind] = index;
    });
    var center = (kinds.length - 1) / 2;
    var spacing = 80;
    items.forEach(function (item) {
      var layer = layerByKind[item.kind] || 0;
      item.fz = (layer - center) * spacing;
      item.__layerIndex = layer + 1;
      item.__layerCount = kinds.length;
    });
    return items;
  };

  TopologyView.prototype.apply = function () {
    var data = this.filteredData();
    this.visibleItems = data.items;
    this.visibleRelations = data.relations;
    this.graph.graphData({ nodes: data.nodes, links: data.links });
    this.renderMeta(data.items, data.relations);
    this.renderLegend(data.items);
    this.renderDetails();
    this.refreshStyles();
  };

  TopologyView.prototype.renderMeta = function (items, relations) {
    var orphanCount = this.items.filter(function (item) {
      return !item.missing && this.degrees[item.id] && this.degrees[item.id].total === 0;
    }, this).length;
    var virtualCount = this.items.filter(function (item) { return item.virtual; }).length;
    this.meta.innerHTML =
      esc(this.items.length) + " items · " + esc(virtualCount) + " virtual · " +
      esc(this.relations.length) + " relations · visible " + esc(items.length) + "/" + esc(relations.length) +
      (orphanCount ? ' · <span class="rye-badge-orphan-text">' + esc(orphanCount) + " orphan</span>" : "") +
      " · layout " + esc(this.state.layoutMode) +
      (this.state.hexGrid ? " · hex" : "") +
      " · events " + esc(this.context.connected ? "connected" : "offline");
  };

  TopologyView.prototype.renderLegend = function (items) {
    var kinds = uniq(items.map(function (item) { return item.kind; }));
    this.legend.innerHTML = '<div class="rye-graph-legend">' + kinds.map(function (kind) {
      return '<span class="rye-legend-item"><span class="rye-legend-dot" style="background:' +
        (COLOR_CSS[kind] || "#a89984") + '"></span>' + esc(kind) + '</span>';
    }).join("") + '</div>';
  };

  TopologyView.prototype.renderDetails = function (selectedRelation) {
    var item = this.itemById[this.state.selectedItemId];
    if (selectedRelation) return this.renderRelationDetails(selectedRelation);
    if (!item) {
      this.details.innerHTML = '<div class="rye-graph-details-header"><div class="rye-graph-title">Select an item</div></div>' +
        '<div class="rye-detail-body rye-graph-meta">Click an item to inspect refs and relations.</div>';
      return;
    }
    var deg = this.degrees[item.id] || { in: 0, out: 0, total: 0 };
    var outgoing = relationList(this.relations, item.id, "out");
    var incoming = relationList(this.relations, item.id, "in");
    var self = this;
    var relationHtml = function (relations, empty) {
      if (!relations.length) return '<div class="rye-graph-meta">' + empty + '</div>';
      return '<ul class="rye-edge-list">' + relations.map(function (rel) {
        var other = rel.from === item.id ? rel.to : rel.from;
        return '<li><button class="rye-relation-link" data-item-id="' + esc(other) + '" data-relation-id="' + esc(rel.id) + '">' +
          esc(rel.type) + ' → ' + esc(other) + '</button></li>';
      }).join("") + '</ul>';
    };
    var status = item.status ? [item.status.resolved ? "resolved" : "unresolved", item.status.composed === true ? "composed" : null, item.status.executable ? "executable" : null].filter(Boolean).join(" · ") : "—";
    this.details.innerHTML =
      '<div class="rye-graph-details-header"><div><div class="rye-graph-title">' + esc(item.label || item.id) +
      (item.virtual ? ' <span class="rye-badge rye-badge-virtual">virtual</span>' : '') +
      (item.missing ? ' <span class="rye-badge rye-badge-untrusted">missing</span>' : '') +
      (deg.total === 0 && !item.missing ? ' <span class="rye-badge rye-badge-orphan">orphan</span>' : '') +
      '</div><div class="rye-graph-meta">' + esc(item.kind) + '</div></div></div>' +
      '<div class="rye-detail-body"><dl class="rye-detail-kv">' +
      '<dt>Ref</dt><dd>' + esc(item.ref || item.id) + '</dd>' +
      '<dt>Space</dt><dd>' + esc(item.space || 'virtual') + '</dd>' +
      '<dt>Status</dt><dd>' + esc(status) + '</dd>' +
      '<dt>Path</dt><dd>' + esc(item.path || '—') + '</dd>' +
      (item.trust ? '<dt>Trust</dt><dd><span class="rye-badge rye-badge-' + esc(item.trust.class) + '">' + esc(item.trust.class) + '</span>' +
        (item.trust.signer ? ' <span class="rye-graph-meta" title="' + esc(item.trust.signer) + '">' + esc(item.trust.signer.substring(0, 12)) + '…</span>' : '') + '</dd>' : '') +
      '<dt>Degree</dt><dd>' + deg.total + ' (in ' + deg.in + ' / out ' + deg.out + ')</dd>' +
      '<dt>Outgoing relations</dt><dd>' + relationHtml(outgoing, 'No outgoing relations') + '</dd>' +
      '<dt>Incoming relations</dt><dd>' + relationHtml(incoming, 'No incoming relations') + '</dd>' +
      '</dl></div>';
    this.details.querySelectorAll(".rye-relation-link").forEach(function (button) {
      button.addEventListener("click", function () {
        var rel = self.relations.find(function (r) { return r.id === button.dataset.relationId; });
        self.selectRelation({ relation: rel });
        self.selectItem(self.itemById[button.dataset.itemId]);
      });
    });
  };

  TopologyView.prototype.renderRelationDetails = function (rel) {
    if (!rel) return;
    this.details.innerHTML = '<div class="rye-graph-details-header"><div><div class="rye-graph-title">relation</div><div class="rye-graph-meta">' + esc(rel.type) + '</div></div></div>' +
      '<div class="rye-detail-body"><dl class="rye-detail-kv">' +
      '<dt>From</dt><dd><button class="rye-relation-link" data-item-id="' + esc(rel.from) + '">' + esc(rel.from) + '</button></dd>' +
      '<dt>To</dt><dd><button class="rye-relation-link" data-item-id="' + esc(rel.to) + '">' + esc(rel.to) + '</button></dd>' +
      '<dt>Confidence</dt><dd>' + esc(rel.confidence || 'structural') + '</dd>' +
      '<dt>Source</dt><dd>' + esc(rel.source && rel.source.path || '—') + '</dd>' +
      '<dt>Field</dt><dd>' + esc(rel.source && rel.source.field || '—') + '</dd>' +
      '</dl></div>';
    var self = this;
    this.details.querySelectorAll(".rye-relation-link").forEach(function (button) {
      button.addEventListener("click", function () { self.selectItem(self.itemById[button.dataset.itemId]); });
    });
  };

  TopologyView.prototype.selectItem = function (item) {
    if (!item) return;
    var real = this.itemById[item.id] || item;
    this.state.selectedItemId = real.id;
    this.apply();
    this.focusCamera(item);
  };

  TopologyView.prototype.selectRelation = function (link) {
    var rel = link && (link.relation || link.edge || link);
    if (rel) this.renderRelationDetails(rel);
  };

  TopologyView.prototype.focusCamera = function (item) {
    if (!item || item.x == null) return;
    var distance = 90;
    var len = Math.hypot(item.x || 1, item.y || 1, item.z || 1) || 1;
    var ratio = 1 + distance / len;
    this.graph.cameraPosition({ x: item.x * ratio, y: item.y * ratio, z: item.z * ratio }, item, 700);
  };

  TopologyView.prototype.nodeLabel = function (item) {
    var deg = this.degrees[item.id];
    var parts = [item.label || item.id, item.kind];
    if (item.virtual) parts.push("virtual");
    if (item.missing) parts.push("missing");
    if (deg && deg.total === 0) parts.push("orphan");
    if (item.__layerIndex) parts.push("layer " + item.__layerIndex + "/" + item.__layerCount);
    return parts.join(" · ");
  };

  TopologyView.prototype.renderNodeColor = function (item) {
    if (item.id === this.state.selectedItemId) return SELECTED_COLOR;
    if (this.matchesSearch(item)) return 0xfabd2f;
    return itemColor(item, this.degrees);
  };

  TopologyView.prototype.renderRelationColor = function (link) {
    var sid = this.state.selectedItemId;
    if (sid && (nodeId(link.source) === sid || nodeId(link.target) === sid)) return "rgba(254,128,25,0.92)";
    return link.relation && link.relation.confidence === "heuristic" ? "rgba(250,189,47,0.26)" : "rgba(168,153,132,0.24)";
  };

  TopologyView.prototype.renderRelationWidth = function (link) {
    var sid = this.state.selectedItemId;
    return sid && (nodeId(link.source) === sid || nodeId(link.target) === sid) ? 1.6 : 0.55;
  };

  TopologyView.prototype.refreshStyles = function () {
    this.graph.nodeColor(this.renderNodeColor.bind(this));
    this.graph.linkColor(this.renderRelationColor.bind(this));
    this.graph.linkWidth(this.renderRelationWidth.bind(this));
  };

  TopologyView.prototype.destroy = function () {
    if (this.graph && typeof this.graph._destructor === "function") this.graph._destructor();
    this.graph = null;
  };

  function render(app, topology, context) {
    var view = instances && instances.get(app);
    if (!view) {
      view = new TopologyView(app, topology, context);
      if (instances) instances.set(app, view);
      return view;
    }
    if (topology && topology !== view.topology) view.setTopology(topology, false);
    view.updateContext(context);
    return view;
  }

  window.RyeGraphView = { render: render };
})();
