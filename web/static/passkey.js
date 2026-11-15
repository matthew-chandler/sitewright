// Passkey enrollment and sign-in (WebAuthn). The server sends and expects base64url;
// the browser API wants ArrayBuffers.
(function () {
  "use strict";
  var csrf = (document.querySelector('meta[name="csrf-token"]') || {}).content || "";
  var status = document.getElementById("passkey-status");

  function show(message, isError) {
    if (!status) return;
    status.innerHTML = '<p class="msg' + (isError ? " error" : "") + '" role="' + (isError ? "alert" : "status") + '"></p>';
    status.firstChild.textContent = message;
  }
  function toBuffer(value) {
    var s = value.replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    var binary = atob(s), bytes = new Uint8Array(binary.length);
    for (var i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes.buffer;
  }
  function fromBuffer(buffer) {
    var bytes = new Uint8Array(buffer), binary = "";
    for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }
  async function post(url, body) {
    var headers = { "Content-Type": "application/json" };
    if (csrf) headers["X-CSRF-Token"] = csrf;
    var response = await fetch(url, { method: "POST", headers: headers, body: JSON.stringify(body), credentials: "same-origin" });
    var data = {};
    try { data = await response.json(); } catch (e) { /* empty body */ }
    if (!response.ok) throw new Error(data.error || "Request failed (" + response.status + ")");
    return data;
  }
  function explain(error) {
    if (error && error.name === "NotAllowedError") return "The passkey prompt was dismissed or timed out. Try again.";
    if (error && error.name === "InvalidStateError") return "This device already has a passkey for Sitewright.";
    if (error && error.name === "SecurityError") return "Passkeys need HTTPS (or localhost). Open the admin UI over https.";
    return (error && error.message) || "Something went wrong.";
  }
  function supported() {
    if (window.PublicKeyCredential && navigator.credentials) return true;
    show("This browser does not support passkeys.", true);
    return false;
  }

  async function register(invite, name) {
    var start = await post("/auth/register/start", { invite: invite || null });
    var options = start.options.publicKey;
    options.challenge = toBuffer(options.challenge);
    options.user.id = toBuffer(options.user.id);
    (options.excludeCredentials || []).forEach(function (c) { c.id = toBuffer(c.id); });
    var created = await navigator.credentials.create({ publicKey: options });
    var credential = {
      id: created.id,
      rawId: fromBuffer(created.rawId),
      type: created.type,
      extensions: created.getClientExtensionResults(),
      response: {
        attestationObject: fromBuffer(created.response.attestationObject),
        clientDataJSON: fromBuffer(created.response.clientDataJSON)
      }
    };
    return post("/auth/register/finish", { ceremony: start.ceremony, credential: credential, name: name || "" });
  }

  async function login() {
    var start = await post("/auth/login/start", {});
    var options = start.options.publicKey;
    options.challenge = toBuffer(options.challenge);
    (options.allowCredentials || []).forEach(function (c) { c.id = toBuffer(c.id); });
    var assertion = await navigator.credentials.get({ publicKey: options });
    var response = assertion.response;
    var credential = {
      id: assertion.id,
      rawId: fromBuffer(assertion.rawId),
      type: assertion.type,
      extensions: assertion.getClientExtensionResults(),
      response: {
        authenticatorData: fromBuffer(response.authenticatorData),
        clientDataJSON: fromBuffer(response.clientDataJSON),
        signature: fromBuffer(response.signature),
        userHandle: response.userHandle ? fromBuffer(response.userHandle) : null
      }
    };
    return post("/auth/login/finish", { ceremony: start.ceremony, credential: credential });
  }

  function run(action, working) {
    if (!supported()) return;
    show(working, false);
    action().then(function (result) {
      window.location.assign(result.redirect || "/");
    }).catch(function (error) { show(explain(error), true); });
  }

  // A SameSite=Strict cookie is not sent with the navigation that opens the installed
  // app or follows a link from elsewhere, so the server may show the sign-in page to
  // someone who is in fact signed in. A same-site request does carry the cookie: ask,
  // and go straight on.
  if (document.getElementById("passkey-login")) {
    fetch("/auth/session", { credentials: "same-origin" })
      .then(function (response) { return response.json(); })
      .then(function (state) { if (state.signed_in) window.location.replace("/"); })
      .catch(function () { /* stay on the sign-in page */ });
  }

  var loginButton = document.getElementById("passkey-login");
  if (loginButton) loginButton.addEventListener("click", function () { run(login, "Waiting for your passkey…"); });

  var enrollForm = document.getElementById("passkey-enroll");
  if (enrollForm) enrollForm.addEventListener("submit", function (event) {
    event.preventDefault();
    var name = enrollForm.querySelector("[name=name]").value;
    run(function () { return register(enrollForm.dataset.invite, name); }, "Creating your passkey…");
  });

  var addForm = document.getElementById("passkey-add");
  if (addForm) addForm.addEventListener("submit", function (event) {
    event.preventDefault();
    var name = addForm.querySelector("[name=name]").value;
    run(function () { return register(null, name); }, "Creating the passkey…");
  });
})();
