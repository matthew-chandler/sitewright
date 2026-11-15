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

  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("/sw.js").catch(function () { /* the app works without it */ });
  }
})();
