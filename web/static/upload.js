// Sitewright admin UI: uploading files to a site from its page.
//
// Files are sent one at a time as the raw request body. Photos too large for the
// per-file limit (and iPhone HEIC photos, which browsers cannot show) are first made
// smaller in the browser, so a picture straight from a phone camera just works.
(function () {
  "use strict";
  var form = document.getElementById("upload-form");
  if (!form) return;
  var csrf = (document.querySelector('meta[name="csrf-token"]') || {}).content || "";
  var input = form.querySelector('input[type="file"]');
  var button = form.querySelector('button[type="submit"]');
  var status = form.querySelector(".status");
  var limit = Number(form.dataset.limit) || 1048576;

  function say(text, isError) {
    status.textContent = "";
    if (!text) return;
    var p = document.createElement("p");
    p.className = isError ? "msg error" : "msg";
    p.setAttribute("role", isError ? "alert" : "status");
    p.textContent = text;
    status.appendChild(p);
  }

  function stem(name) {
    var dot = name.lastIndexOf(".");
    return dot > 0 ? name.slice(0, dot) : name;
  }

  function toBlob(canvas, type, quality) {
    return new Promise(function (resolve) { canvas.toBlob(resolve, type, quality); });
  }

  // Draw the image at most `edge` pixels on its long side and encode it. WebP keeps
  // transparency; browsers that cannot encode it get JPEG on a white background.
  async function encode(bitmap, edge) {
    var scale = Math.min(1, edge / Math.max(bitmap.width, bitmap.height));
    var canvas = document.createElement("canvas");
    canvas.width = Math.max(1, Math.round(bitmap.width * scale));
    canvas.height = Math.max(1, Math.round(bitmap.height * scale));
    var ctx = canvas.getContext("2d");
    ctx.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
    var blob = await toBlob(canvas, "image/webp", 0.85);
    if (blob && blob.type === "image/webp") return { blob: blob, ext: "webp" };
    ctx.fillStyle = "#fff";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
    blob = await toBlob(canvas, "image/jpeg", 0.85);
    return blob ? { blob: blob, ext: "jpg" } : null;
  }

  // The file to send: the original, or a smaller re-encoded copy of a photo.
  async function prepare(file) {
    var heic = /\.(heic|heif)$/i.test(file.name) || /image\/hei[cf]/i.test(file.type);
    var raster = heic || /^image\/(jpeg|png|webp)$/i.test(file.type);
    if (!raster || (!heic && file.size <= limit) || !window.createImageBitmap) {
      return { blob: file, name: file.name };
    }
    var bitmap;
    try {
      bitmap = await createImageBitmap(file);
    } catch (e) {
      return { blob: file, name: file.name }; // the server explains what is wrong
    }
    var edges = [2400, 1800, 1400, 1000, 700];
    var out = null;
    for (var i = 0; i < edges.length; i++) {
      out = await encode(bitmap, edges[i]);
      if (out && out.blob.size <= limit) break;
    }
    if (bitmap.close) bitmap.close();
    if (!out) return { blob: file, name: file.name };
    return { blob: out.blob, name: stem(file.name) + "." + out.ext };
  }

  function errorText(response, body) {
    if (response.status === 413) return "the file is larger than the limit";
    try { return JSON.parse(body).error || body; } catch (e) { return body || ("the server answered " + response.status); }
  }

  form.addEventListener("submit", async function (event) {
    event.preventDefault();
    var files = Array.prototype.slice.call(input.files || []);
    if (!files.length) return;
    button.disabled = true;
    var failures = [];
    var done = 0;
    for (var i = 0; i < files.length; i++) {
      var file = files[i];
      var prefix = files.length > 1 ? (i + 1) + " of " + files.length + ": " : "";
      try {
        say(prefix + "preparing " + file.name + "…");
        var prepared = await prepare(file);
        say(prefix + "uploading " + prepared.name + "…");
        var response = await fetch(form.dataset.url + "?name=" + encodeURIComponent(prepared.name), {
          method: "PUT",
          credentials: "same-origin",
          headers: { "X-CSRF-Token": csrf, "Content-Type": prepared.blob.type || "application/octet-stream" },
          body: prepared.blob
        });
        var body = await response.text();
        if (!response.ok) {
          failures.push(file.name + ": " + errorText(response, body));
          continue;
        }
        done++;
        var current = document.getElementById("uploads");
        if (current) {
          current.outerHTML = body;
          var fresh = document.getElementById("uploads");
          if (fresh && window.htmx) window.htmx.process(fresh);
        }
      } catch (e) {
        failures.push(file.name + ": could not reach the server");
      }
    }
    button.disabled = false;
    input.value = "";
    if (failures.length) {
      say((done ? "Uploaded " + done + ", but not: " : "Not uploaded: ") + failures.join(" · "), true);
    } else {
      say("");
    }
  });

  // "Use" puts an upload's path into the prompt box (or, when a run is in progress and
  // there is no prompt box, copies it).
  document.body.addEventListener("click", function (event) {
    var target = event.target;
    if (!target || !target.dataset || !target.dataset.insert) return;
    var path = target.dataset.insert;
    var prompt = document.getElementById("prompt");
    if (!prompt) {
      if (navigator.clipboard) navigator.clipboard.writeText(path);
      return;
    }
    var value = prompt.value;
    prompt.value = value + (value && !/\s$/.test(value) ? " " : "") + path + " ";
    prompt.focus();
    prompt.scrollIntoView({ block: "center" });
  });
})();
