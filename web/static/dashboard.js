// Renders a site's dashboards. The server sends each panel with its rows; charts are
// drawn with Vega-Lite using the expression interpreter, so no eval is needed and the
// page's Content-Security-Policy stays strict.
(function () {
  "use strict";
  var standard = document.getElementById("dash-standard");
  if (!standard) return;
  var custom = document.getElementById("dash-custom");
  var days = document.getElementById("dash-days");
  var site = standard.dataset.site;
  var dark = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
  var ink = dark ? "#e9ecea" : "#1b1f1d", faint = dark ? "#2f3633" : "#dcdfdb", accent = dark ? "#58c39d" : "#1d6b52";
  var theme = {
    background: "transparent",
    font: "system-ui, sans-serif",
    axis: { labelColor: ink, titleColor: ink, gridColor: faint, domainColor: faint, tickColor: faint, labelLimit: 160 },
    legend: { labelColor: ink, titleColor: ink },
    title: { color: ink },
    view: { stroke: "transparent" },
    mark: { color: accent },
    range: { category: [accent, "#d98a3d", "#6c8fd6", "#c96a8b", "#8a9a5b", "#9a7fc7"] }
  };

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text) node.textContent = text;
    return node;
  }

  function draw(container, panels) {
    container.textContent = "";
    panels.forEach(function (panel) {
      var card = el("article", "panel");
      card.appendChild(el("h3", "", panel.title));
      if (panel.description) card.appendChild(el("p", "muted", panel.description));
      var chart = el("div", "chart");
      card.appendChild(chart);
      var details = el("details", "sql");
      details.appendChild(el("summary", "", "Show SQL"));
      details.appendChild(el("pre", "", panel.sql));
      card.appendChild(details);
      container.appendChild(card);

      if (panel.error) {
        chart.appendChild(el("p", "msg error", panel.error));
      } else if (!panel.rows.length) {
        chart.appendChild(el("p", "empty", "No data in this period yet."));
      } else {
        var spec = Object.assign({}, panel.vega_lite, { data: { values: panel.rows }, width: "container" });
        if (!spec.height) spec.height = 200;
        vegaEmbed(chart, spec, {
          actions: false, renderer: "svg", ast: true, expr: vegaInterpreter.expressionInterpreter, config: theme
        }).catch(function (error) {
          chart.textContent = "";
          chart.appendChild(el("p", "msg error", "This chart could not be drawn: " + error.message));
        });
      }
    });
  }

  function load() {
    fetch("/sites/" + site + "/dashboard/data?days=" + days.value, { credentials: "same-origin" })
      .then(function (response) { if (!response.ok) throw new Error("status " + response.status); return response.json(); })
      .then(function (data) { draw(custom, data.custom); draw(standard, data.standard); })
      .catch(function () { standard.textContent = ""; standard.appendChild(el("p", "msg error", "Could not load the dashboard.")); });
  }
  days.addEventListener("change", load);
  load();
})();
