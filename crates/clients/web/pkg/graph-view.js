// Minimal RyeOS topology graph renderer.
//
// This intentionally consumes the daemon's topology object and keeps graph
// semantics in Rust. The renderer is a small no-dependency bridge until the
// vendored 3D graph library lands.

(function () {
  "use strict";

  var COLORS = {
    directive: "#7decff",
    graph: "#b48cff",
    graph_node: "#d5c3ff",
    knowledge: "#8cffba",
    tool: "#ffd36b",
    surface: "#ff8fc7",
    client: "#ffad7d",
    parser: "#84a0ff",
    handler: "#a7b5df",
    runtime: "#ff6f91",
    kind_schema: "#ffffff",
  };

  function esc(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function nodeColor(node) {
    return COLORS[node.kind] || "#9aa9d8";
  }

  function layout(nodes, width, height) {
    var byKind = {};
    nodes.forEach(function (node) {
      (byKind[node.kind] || (byKind[node.kind] = [])).push(node);
    });

    var kinds = Object.keys(byKind).sort();
    var centerX = width / 2;
    var centerY = height / 2;
    var maxRadius = Math.max(90, Math.min(width, height) * 0.42);
    var positions = {};

    kinds.forEach(function (kind, kindIndex) {
      var kindAngle = (Math.PI * 2 * kindIndex) / Math.max(kinds.length, 1);
      var clusterX = centerX + Math.cos(kindAngle) * maxRadius * 0.48;
      var clusterY = centerY + Math.sin(kindAngle) * maxRadius * 0.48;
      var group = byKind[kind];
      var radius = Math.max(28, Math.min(110, 20 + group.length * 4));
      group.forEach(function (node, i) {
        var angle = (Math.PI * 2 * i) / Math.max(group.length, 1);
        positions[node.id] = {
          x: clusterX + Math.cos(angle) * radius,
          y: clusterY + Math.sin(angle) * radius,
        };
      });
    });

    return positions;
  }

  function edgeList(edges, nodeId, direction) {
    return edges.filter(function (edge) {
      return direction === "out" ? edge.from === nodeId : edge.to === nodeId;
    });
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
          '<option value="' +
          esc(value) +
          '"' +
          (value === selected ? " selected" : "") +
          ">" +
          esc(value) +
          "</option>"
        );
      })
      .join("");
  }

  function renderDetails(container, graph, node) {
    if (!node) {
      container.innerHTML =
        '<div class="rye-graph-details-header"><div class="rye-graph-title">Select a node</div></div>' +
        '<div class="rye-detail-body rye-graph-meta">Click a node to inspect refs and relationships.</div>';
      return;
    }

    var outgoing = edgeList(graph.edges || [], node.id, "out");
    var incoming = edgeList(graph.edges || [], node.id, "in");
    var edgeHtml = function (edges, empty) {
      if (!edges.length) return '<div class="rye-graph-meta">' + empty + "</div>";
      return (
        '<ul class="rye-edge-list">' +
        edges
          .map(function (edge) {
            var other = edge.from === node.id ? edge.to : edge.from;
            return "<li>" + esc(edge.type) + " → " + esc(other) + "</li>";
          })
          .join("") +
        "</ul>"
      );
    };

    container.innerHTML =
      '<div class="rye-graph-details-header">' +
      '<div class="rye-graph-title">' +
      esc(node.label || node.id) +
      "</div>" +
      '<div class="rye-graph-meta">' +
      esc(node.kind) +
      "</div>" +
      "</div>" +
      '<div class="rye-detail-body">' +
      '<dl class="rye-detail-kv">' +
      "<dt>Ref</dt><dd>" +
      esc(node.ref || node.id) +
      "</dd>" +
      "<dt>Space</dt><dd>" +
      esc(node.space || "—") +
      "</dd>" +
      "<dt>Path</dt><dd>" +
      esc(node.path || "—") +
      "</dd>" +
      "<dt>Outgoing</dt><dd>" +
      edgeHtml(outgoing, "No outgoing edges") +
      "</dd>" +
      "<dt>Incoming</dt><dd>" +
      edgeHtml(incoming, "No incoming edges") +
      "</dd>" +
      "</dl>" +
      "</div>";
  }

  function render(app, graph, context) {
    var nodes = graph.nodes || [];
    var edges = graph.edges || [];
    var selectedId = nodes.length ? nodes[0].id : null;
    var filters = {
      query: "",
      kind: "all",
      edgeType: "all",
      localOnly: false,
    };
    var width = Math.max(720, app.clientWidth - 380);
    var height = Math.max(520, window.innerHeight - 112);
    var positions = layout(nodes, width, height);
    var kinds = uniq(nodes.map(function (node) { return node.kind; }));
    var edgeTypes = uniq(edges.map(function (edge) { return edge.type; }));

    app.innerHTML =
      '<div class="rye-graph-shell">' +
      '<section class="rye-graph-main">' +
      '<div class="rye-graph-header">' +
      '<div>' +
      '<div class="rye-graph-title">RyeOS topology</div>' +
      '<div class="rye-graph-meta">' +
      esc(nodes.length) +
      " nodes · " +
      esc(edges.length) +
      " edges · events " +
      esc(context && context.connected ? "connected" : "offline") +
      "</div>" +
      "</div>" +
      '<div class="rye-graph-controls">' +
      '<input id="rye-graph-search" type="search" placeholder="Search refs…" />' +
      '<select id="rye-graph-kind"><option value="all">all kinds</option>' +
      optionList(kinds, filters.kind) +
      "</select>" +
      '<select id="rye-graph-edge-type"><option value="all">all edges</option>' +
      optionList(edgeTypes, filters.edgeType) +
      "</select>" +
      '<label><input id="rye-graph-local" type="checkbox" /> local</label>' +
      "</div>" +
      "</div>" +
      '<svg class="rye-graph-canvas" viewBox="0 0 ' +
      width +
      " " +
      height +
      '" role="img" aria-label="RyeOS topology graph"></svg>' +
      "</section>" +
      '<aside class="rye-graph-details" id="rye-graph-details"></aside>' +
      "</div>";

    var svg = app.querySelector("svg");
    var details = app.querySelector("#rye-graph-details");
    var search = app.querySelector("#rye-graph-search");
    var kindSelect = app.querySelector("#rye-graph-kind");
    var edgeTypeSelect = app.querySelector("#rye-graph-edge-type");
    var localToggle = app.querySelector("#rye-graph-local");
    var nodeById = {};
    nodes.forEach(function (node) {
      nodeById[node.id] = node;
    });

    function matchesQuery(node) {
      if (!filters.query) return true;
      var haystack = [node.id, node.ref, node.label, node.kind, node.namespace]
        .join(" ")
        .toLowerCase();
      return haystack.indexOf(filters.query.toLowerCase()) !== -1;
    }

    function relatedToSelected(node) {
      if (!filters.localOnly || !selectedId) return true;
      if (node.id === selectedId) return true;
      return edges.some(function (edge) {
        return (
          (edge.from === selectedId && edge.to === node.id) ||
          (edge.to === selectedId && edge.from === node.id)
        );
      });
    }

    function filteredGraph() {
      var visible = {};
      var visibleNodes = nodes.filter(function (node) {
        var include =
          (filters.kind === "all" || node.kind === filters.kind) &&
          matchesQuery(node) &&
          relatedToSelected(node);
        if (include) visible[node.id] = true;
        return include;
      });
      var visibleEdges = edges.filter(function (edge) {
        return (
          visible[edge.from] &&
          visible[edge.to] &&
          (filters.edgeType === "all" || edge.type === filters.edgeType)
        );
      });
      return { nodes: visibleNodes, edges: visibleEdges };
    }

    function redraw() {
      var visible = filteredGraph();
      svg.innerHTML = "";
      visible.edges.forEach(function (edge) {
        var a = positions[edge.from];
        var b = positions[edge.to];
        if (!a || !b) return;
        var line = document.createElementNS("http://www.w3.org/2000/svg", "line");
        line.setAttribute("x1", a.x);
        line.setAttribute("y1", a.y);
        line.setAttribute("x2", b.x);
        line.setAttribute("y2", b.y);
        line.setAttribute(
          "class",
          "rye-edge" +
            (edge.from === selectedId || edge.to === selectedId ? " active" : "")
        );
        svg.appendChild(line);
      });

      visible.nodes.forEach(function (node) {
        var p = positions[node.id];
        if (!p) return;
        var group = document.createElementNS("http://www.w3.org/2000/svg", "g");
        group.setAttribute("class", "rye-node" + (node.id === selectedId ? " active" : ""));
        group.setAttribute("transform", "translate(" + p.x + " " + p.y + ")");
        group.addEventListener("click", function () {
          selectedId = node.id;
          redraw();
        });

        var circle = document.createElementNS("http://www.w3.org/2000/svg", "circle");
        circle.setAttribute("r", node.kind === "kind_schema" ? 9 : 7);
        circle.setAttribute("fill", nodeColor(node));
        group.appendChild(circle);

        var text = document.createElementNS("http://www.w3.org/2000/svg", "text");
        text.setAttribute("x", 11);
        text.setAttribute("y", 4);
        text.textContent = node.label || node.id;
        group.appendChild(text);

        svg.appendChild(group);
      });

      renderDetails(details, graph, nodeById[selectedId]);
    }

    search.addEventListener("input", function () {
      filters.query = search.value.trim();
      redraw();
    });
    kindSelect.addEventListener("change", function () {
      filters.kind = kindSelect.value;
      redraw();
    });
    edgeTypeSelect.addEventListener("change", function () {
      filters.edgeType = edgeTypeSelect.value;
      redraw();
    });
    localToggle.addEventListener("change", function () {
      filters.localOnly = localToggle.checked;
      redraw();
    });

    redraw();
  }

  window.RyeGraphView = { render: render };
})();
