// Sitewright admin UI: shared behaviour. No inline scripts anywhere (see the CSP).
(function () {
  "use strict";
  var csrf = (document.querySelector('meta[name="csrf-token"]') || {}).content || "";

  // Every htmx request carries the session's CSRF token.
  document.body.addEventListener("htmx:configRequest", function (event) {
    if (csrf) event.detail.headers["X-CSRF-Token"] = csrf;
  });
  // Show failures instead of failing silently.
  document.body.addEventListener("htmx:responseError", function (event) {
    var target = event.detail.target;
    var message = "Request failed (" + event.detail.xhr.status + ").";
    try { message = JSON.parse(event.detail.xhr.responseText).error || message; } catch (e) { /* not JSON */ }
    if (target) target.innerHTML = '<p class="msg error" role="alert"></p>', target.firstChild.textContent = message;
  });
  document.body.addEventListener("htmx:sendError", function (event) {
    var target = event.detail.target;
    if (target) target.innerHTML = '<p class="msg error" role="alert">Could not reach the server.</p>';
  });

  // Ctrl+Enter (Cmd+Enter on a Mac) in a text box submits its form.
  document.body.addEventListener("keydown", function (event) {
    if (event.key !== "Enter" || !(event.ctrlKey || event.metaKey)) return;
    var target = event.target;
    if (!target || target.tagName !== "TEXTAREA" || !target.form) return;
    event.preventDefault();
    if (target.form.requestSubmit) target.form.requestSubmit(); else target.form.submit();
  });

  // New site: suggest a name from the prompt until the user edits the name.
  var prompt = document.getElementById("prompt");
  var name = document.getElementById("name");
  if (prompt && name) {
    var edited = false;
    var stop = { a: 1, an: 1, the: 1, my: 1, our: 1, with: 1, for: 1, and: 1, of: 1, about: 1, site: 1, website: 1, page: 1, that: 1, to: 1, in: 1, on: 1, me: 1, make: 1, build: 1, create: 1 };
    name.addEventListener("input", function () { edited = name.value !== ""; });
    prompt.addEventListener("input", function () {
      if (edited) return;
      var words = prompt.value.toLowerCase().replace(/[^a-z0-9\s-]/g, " ").split(/\s+/).filter(function (w) { return w && !stop[w]; });
      name.value = words.slice(0, 2).join("-").slice(0, 40).replace(/^-+|-+$/g, "");
    });
  }

  // Searchable dropdowns: a search box with data-filter-select="<select id>" narrows
  // that select to the options containing every word typed. The full list is kept
  // aside and the options are rebuilt, because hiding options does not work on iOS.
  var allOptions = {};
  function optionsOf(select) {
    if (!allOptions[select.id] || allOptions[select.id].owner !== select) {
      allOptions[select.id] = {
        owner: select,
        items: Array.prototype.map.call(select.options, function (o) { return { value: o.value, text: o.textContent }; })
      };
    }
    return allOptions[select.id].items;
  }
  function suggestId(select) {
    var field = document.querySelector('[data-id-from="' + select.id + '"]');
    if (!field || field.dataset.edited || !select.value) return;
    field.value = ("copilot-" + select.value).toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/-+/g, "-").replace(/^-|-$/g, "").slice(0, 40);
  }
  document.body.addEventListener("input", function (event) {
    var box = event.target;
    if (box && box.dataset && box.dataset.idFrom) { box.dataset.edited = box.value ? "1" : ""; return; }
    if (!box || !box.dataset || !box.dataset.filterSelect) return;
    var select = document.getElementById(box.dataset.filterSelect);
    if (!select) return;
    var items = optionsOf(select);
    var words = box.value.toLowerCase().split(/\s+/).filter(Boolean);
    var matches = items.filter(function (item) {
      var hay = (item.text + " " + item.value).toLowerCase();
      return words.every(function (w) { return hay.indexOf(w) !== -1; });
    });
    var previous = select.value;
    select.textContent = "";
    matches.forEach(function (item) {
      var option = document.createElement("option");
      option.value = item.value;
      option.textContent = item.text;
      select.appendChild(option);
    });
    if (matches.some(function (m) { return m.value === previous; })) select.value = previous;
    var count = document.querySelector('[data-filter-count="' + select.id + '"]');
    if (count) count.textContent = words.length ? matches.length + " of " + items.length + " match" : "";
    suggestId(select);
  });
  document.body.addEventListener("change", function (event) {
    if (event.target && event.target.tagName === "SELECT") suggestId(event.target);
  });
  document.body.addEventListener("htmx:afterSwap", function () {
    var select = document.getElementById("cm-model");
    if (select) suggestId(select);
  });

  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("/sw.js").catch(function () { /* the app works without it */ });
  }
})();
