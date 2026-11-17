// Sitewright admin UI: uploading files to a site from its page.
//
// Files are sent one at a time as the raw request body. There is no size limit, but
// files over the warning size (15 MB) need a confirmation first, because they make
// pages slow to load. iPhone HEIC photos, which browsers cannot show, are converted to
// WebP (or JPEG) in the browser before they are sent.
(function () {
  "use strict";
  var form = document.getElementById("upload-form");
  if (!form) return;
  var csrf = (document.querySelector('meta[name="csrf-token"]') || {}).content || "";
  var input = form.querySelector('input[type="file"]');
  var button = form.querySelector('button[type="submit"]');
  var status = form.querySelector(".status");
  var warnBytes = Number(form.dataset.warn) || 15 * 1024 * 1024;

  function megabytes(bytes) {
    return (bytes / (1024 * 1024)).toFixed(bytes < 100 * 1024 * 1024 ? 1 : 0) + " MB";
  }

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
    var blob = await toBlob(canvas, "image/webp", 0.9);
    if (blob && blob.type === "image/webp") return { blob: blob, ext: "webp" };
    ctx.fillStyle = "#fff";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
    blob = await toBlob(canvas, "image/jpeg", 0.9);
    return blob ? { blob: blob, ext: "jpg" } : null;
  }

  // The file to send: the original, or for a HEIC photo a converted copy. 4096 pixels
  // on the long side keeps full detail for the web and stays within what phones allow
  // a canvas to be.
  async function prepare(file) {
    var heic = /\.(heic|heif)$/i.test(file.name) || /image\/hei[cf]/i.test(file.type);
    if (!heic || !window.createImageBitmap) return { blob: file, name: file.name };
    var bitmap;
    try {
      bitmap = await createImageBitmap(file);
    } catch (e) {
      return { blob: file, name: file.name }; // the server explains what is wrong
    }
    var out = await encode(bitmap, 4096);
    if (bitmap.close) bitmap.close();
    if (!out) return { blob: file, name: file.name };
    return { blob: out.blob, name: stem(file.name) + "." + out.ext };
  }

  var PART = 8 * 1024 * 1024; // under any usual proxy limit (nginx examples: 25 MB, Cloudflare: 100 MB)

  // One PUT. Resolves with { ok, status, text }; rejects only when the server cannot
  // be reached. `onProgress` gets the bytes of this request sent so far.
  function put(url, body, onProgress) {
    return new Promise(function (resolve, reject) {
      var xhr = new XMLHttpRequest();
      xhr.open("PUT", url);
      xhr.setRequestHeader("X-CSRF-Token", csrf);
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      if (onProgress) xhr.upload.onprogress = function (event) { onProgress(event.loaded); };
      xhr.onload = function () {
        resolve({ ok: xhr.status >= 200 && xhr.status < 300, status: xhr.status, text: xhr.responseText });
      };
      xhr.onerror = function () { reject(new Error("network")); };
      xhr.send(body);
    });
  }

  function newId() {
    var bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    return Array.prototype.map.call(bytes, function (b) { return ("0" + b.toString(16)).slice(-2); }).join("");
  }

  function pause(ms) { return new Promise(function (resolve) { setTimeout(resolve, ms); }); }

  // Send a file in parts of PART bytes, then ask the server to store it. A part that
  // fails to arrive is sent again, up to three times, from where the server has it.
  async function send(prepared, prefix) {
    var blob = prepared.blob;
    var total = blob.size;
    var id = newId();
    var base = form.dataset.url;
    var offset = 0;
    var tries = 0;
    while (offset < total) {
      var part = blob.slice(offset, Math.min(offset + PART, total));
      var start = offset;
      var response;
      try {
        response = await put(base + "/parts/" + id + "?offset=" + start, part, function (loaded) {
          var pct = Math.floor(((start + loaded) / total) * 100);
          say(prefix + "uploading " + prepared.name + ": " + Math.min(pct, 99) + "% of " + megabytes(total) + "…");
        });
      } catch (e) {
        response = null;
      }
      if (response && response.ok) {
        offset = Number(response.text) || (start + part.size);
        tries = 0;
        continue;
      }
      if (response && response.status === 409) {
        offset = Number(response.text) || 0; // the server says where to carry on
        continue;
      }
      if (response && response.status !== 0 && response.status < 500) return response;
      if (++tries > 3) {
        if (response) return response;
        throw new Error("network");
      }
      say(prefix + "connection lost; trying " + prepared.name + " again…");
      await pause(2000 * tries);
    }
    say(prefix + "saving and publishing " + prepared.name + "…");
    return put(base + "?name=" + encodeURIComponent(prepared.name) + "&upload=" + id, null, null);
  }

  function errorText(response, body) {
    if (response.status === 413) return "a proxy in front of Sitewright refused a file this large";
    try { return JSON.parse(body).error || body; } catch (e) { return body || ("the server answered " + response.status); }
  }

  form.addEventListener("submit", async function (event) {
    event.preventDefault();
    var files = Array.prototype.slice.call(input.files || []);
    if (!files.length) return;
    var failures = [];
    // Large files are allowed, but only on purpose.
    var large = files.filter(function (f) { return f.size > warnBytes; });
    if (large.length) {
      var list = large.map(function (f) { return "\u2022 " + f.name + " (" + megabytes(f.size) + ")"; }).join("\n");
      var ok = window.confirm(
        (large.length === 1 ? "This file is" : "These files are") + " larger than " + megabytes(warnBytes) + ":\n\n" + list +
        "\n\nLarge files make the site slow to load, especially on phones, and take space on the server. Upload " +
        (large.length === 1 ? "it" : "them") + " anyway?"
      );
      if (!ok) {
        files = files.filter(function (f) { return f.size <= warnBytes; });
        large.forEach(function (f) { failures.push(f.name + ": skipped"); });
        if (!files.length) {
          say("Nothing uploaded.", false);
          return;
        }
      }
    }
    button.disabled = true;
    var done = 0;
    for (var i = 0; i < files.length; i++) {
      var file = files[i];
      var prefix = files.length > 1 ? (i + 1) + " of " + files.length + ": " : "";
      try {
        say(prefix + "preparing " + file.name + "…");
        var prepared = await prepare(file);
        say(prefix + "uploading " + prepared.name + "…");
        var response = await send(prepared, prefix);
        var body = response.text;
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
