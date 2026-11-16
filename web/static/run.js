// Live view of an agent run: replays stored events, then follows the run over SSE.
(function () {
  "use strict";
  var log = document.getElementById("run-log");
  if (!log) return;
  var statusBadge = document.getElementById("run-status");
  var usage = document.getElementById("run-usage");
  var streaming = null; // the element receiving text deltas for the current turn

  function add(className, text) {
    var el = document.createElement("div");
    el.className = className;
    el.textContent = text;
    log.appendChild(el);
    el.scrollIntoView({ block: "nearest" });
    return el;
  }
  function describe(args) {
    if (!args || typeof args !== "object") return "";
    var keys = ["path", "message", "hostname", "title", "release", "name"];
    for (var i = 0; i < keys.length; i++) if (typeof args[keys[i]] === "string") return " " + args[keys[i]];
    return "";
  }
  // Start time and a running clock. Times come from the server, so a reload keeps them.
  var clock = document.getElementById("run-clock");
  var started = Number(clock.dataset.started);
  var finished = Number(clock.dataset.finished) || 0;
  var elapsed = document.getElementById("run-elapsed");
  var timer = null;
  function duration(ms) {
    var s = Math.max(0, Math.round(ms / 1000));
    var h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
    var two = function (n) { return (n < 10 ? "0" : "") + n; };
    return h ? h + ":" + two(m) + ":" + two(sec) : m + ":" + two(sec);
  }
  function tick() {
    elapsed.textContent = finished ? "took " + duration(finished - started) : duration(Date.now() - started) + " elapsed";
  }
  (function () {
    var when = new Date(started);
    var el = document.getElementById("run-started");
    el.dateTime = when.toISOString();
    var sameDay = when.toDateString() === new Date().toDateString();
    el.textContent = sameDay
      ? when.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
      : when.toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
    tick();
    if (!finished) timer = setInterval(tick, 1000);
  })();

  // Show a complete message: server-rendered Markdown when there is some.
  function showText(el, event) {
    if (typeof event.html === "string") {
      el.className = "text md";
      el.innerHTML = event.html;
      var links = el.querySelectorAll("a");
      for (var i = 0; i < links.length; i++) { links[i].target = "_blank"; links[i].rel = "noopener noreferrer"; }
    } else {
      el.textContent = event.text;
    }
  }

  function finish(status, message) {
    if (!finished) { finished = Date.now(); }
    if (timer) clearInterval(timer);
    tick();
    statusBadge.textContent = status;
    statusBadge.className = "badge " + status;
    if (status !== "succeeded") add("done", message || status);
    var cancel = document.getElementById("run-cancel");
    if (cancel) cancel.hidden = true;
    document.getElementById("run-open").classList.add("primary");
    var hint = document.getElementById("run-hint");
    if (hint) hint.hidden = true;
  }

  function handle(event) {
    switch (event.type) {
      case "text_delta":
        if (!streaming) streaming = add("text", "");
        streaming.textContent += event.delta;
        break;
      case "text":
        showText(streaming || add("text", ""), event);
        streaming = null;
        break;
      case "tool_call": {
        streaming = null;
        var el = add("tool", "");
        var name = document.createElement("span");
        name.className = "name";
        name.textContent = event.name;
        el.appendChild(name);
        el.appendChild(document.createTextNode(describe(event.arguments)));
        el.dataset.call = event.id;
        break;
      }
      case "tool_result":
        if (event.is_error) {
          var message = event.content;
          try { message = JSON.parse(event.content).error || message; } catch (e) { /* plain text */ }
          add("tool error", event.name + " failed: " + message);
        }
        break;
      case "usage":
        usage.textContent = "· " + event.input_tokens + " tokens in, " + event.output_tokens + " out" +
          (typeof event.cost_usd === "number" ? " · $" + event.cost_usd.toFixed(4) : "");
        break;
      case "confirmation":
        if (event.action === "delete_site") document.getElementById("run-confirm").hidden = false;
        break;
      case "done":
        finish(event.status, event.message);
        return true;
    }
    return false;
  }

  var source = new EventSource("/runs/" + log.dataset.run + "/events");
  source.onmessage = function (message) {
    var done = false;
    try { done = handle(JSON.parse(message.data)); } catch (e) { /* ignore malformed events */ }
    if (done) source.close();
  };
  source.onerror = function () {
    // The server closes the stream when the run ends; EventSource would reconnect and
    // replay everything, so stop here. A reload shows the stored history.
    source.close();
    if (statusBadge.textContent === "running") add("done", "Connection lost. Reload to see the latest.");
  };
})();
