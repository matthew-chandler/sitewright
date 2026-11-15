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
  function finish(status, message) {
    statusBadge.textContent = status;
    statusBadge.className = "badge " + status;
    if (status !== "succeeded") add("done", message || status);
    var cancel = document.getElementById("run-cancel");
    if (cancel) cancel.hidden = true;
    document.getElementById("run-open").hidden = false;
  }

  function handle(event) {
    switch (event.type) {
      case "text_delta":
        if (!streaming) streaming = add("text", "");
        streaming.textContent += event.delta;
        break;
      case "text":
        if (streaming) { streaming.textContent = event.text; streaming = null; }
        else add("text", event.text);
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
