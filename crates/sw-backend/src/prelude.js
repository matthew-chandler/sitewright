// The "sitewright" module: everything a site backend can import.
//
// It runs inside the jailed worker. The only way out is `host.call`, a message to the
// daemon, which checks every request against the site it belongs to.

const host = globalThis.__host;
delete globalThis.__host;

function call(op, args) {
  return JSON.parse(host.call(op, JSON.stringify(args === undefined ? null : args)));
}

// ---------------------------------------------------------------- console

function show(value) {
  if (typeof value === "string") return value;
  if (value instanceof Error) return value.stack ? `${value.message}\n${value.stack}` : String(value);
  try {
    const text = JSON.stringify(value);
    return text === undefined ? String(value) : text;
  } catch (_) {
    return String(value);
  }
}
function logger(level) {
  return (...args) => host.log(level, args.map(show).join(" "));
}
export const log = { debug: logger("debug"), info: logger("info"), warn: logger("warn"), error: logger("error") };
globalThis.console = { log: log.info, info: log.info, debug: log.debug, warn: log.warn, error: log.error };

// ---------------------------------------------------------------- randomness

export const crypto = {
  /** `n` random bytes as a lowercase hex string (n ≤ 1024). */
  randomHex(n = 16) {
    return host.random(n);
  },
  randomUUID() {
    const h = host.random(16).split("");
    h[12] = "4";
    h[16] = "89ab"[parseInt(h[16], 16) & 3];
    const s = h.join("");
    return `${s.slice(0, 8)}-${s.slice(8, 12)}-${s.slice(12, 16)}-${s.slice(16, 20)}-${s.slice(20)}`;
  },
};
globalThis.crypto = crypto;

// ---------------------------------------------------------------- HTTP types

function decode(text) {
  try {
    return decodeURIComponent(text.replace(/\+/g, " "));
  } catch (_) {
    return text;
  }
}

export class URLSearchParams {
  constructor(init = "") {
    this._pairs = [];
    if (typeof init === "string") {
      for (const part of init.replace(/^\?/, "").split("&")) {
        if (!part) continue;
        const eq = part.indexOf("=");
        this._pairs.push(eq < 0 ? [decode(part), ""] : [decode(part.slice(0, eq)), decode(part.slice(eq + 1))]);
      }
    } else if (init && typeof init === "object") {
      for (const key of Object.keys(init)) this._pairs.push([key, String(init[key])]);
    }
  }
  get(name) {
    const pair = this._pairs.find((p) => p[0] === name);
    return pair ? pair[1] : null;
  }
  getAll(name) {
    return this._pairs.filter((p) => p[0] === name).map((p) => p[1]);
  }
  has(name) {
    return this._pairs.some((p) => p[0] === name);
  }
  append(name, value) {
    this._pairs.push([String(name), String(value)]);
  }
  entries() {
    return this._pairs.map((p) => [p[0], p[1]])[Symbol.iterator]();
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  /** A plain object; when a name repeats, the first value wins. */
  toObject() {
    const out = {};
    for (const [k, v] of this._pairs) if (!(k in out)) out[k] = v;
    return out;
  }
  toString() {
    return this._pairs.map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`).join("&");
  }
}
globalThis.URLSearchParams = URLSearchParams;

export class Headers {
  constructor(init) {
    this._list = [];
    if (init instanceof Headers) this._list = init._list.map((p) => [p[0], p[1]]);
    else if (Array.isArray(init)) for (const [k, v] of init) this.append(k, v);
    else if (init && typeof init === "object") for (const k of Object.keys(init)) this.append(k, init[k]);
  }
  get(name) {
    const key = String(name).toLowerCase();
    const values = this._list.filter((p) => p[0] === key).map((p) => p[1]);
    return values.length ? values.join(", ") : null;
  }
  has(name) {
    const key = String(name).toLowerCase();
    return this._list.some((p) => p[0] === key);
  }
  set(name, value) {
    this.delete(name);
    this.append(name, value);
  }
  append(name, value) {
    this._list.push([String(name).toLowerCase(), String(value)]);
  }
  delete(name) {
    const key = String(name).toLowerCase();
    this._list = this._list.filter((p) => p[0] !== key);
  }
  entries() {
    return this._list.map((p) => [p[0], p[1]])[Symbol.iterator]();
  }
  [Symbol.iterator]() {
    return this.entries();
  }
}
globalThis.Headers = Headers;

/** Throw this to answer with a specific status: `throw new HttpError(400, "name is required")`. */
export class HttpError extends Error {
  constructor(status, message) {
    super(message || `HTTP ${status}`);
    this.status = status;
  }
}

export class Request {
  constructor(init) {
    this.method = init.method;
    this.path = init.path;
    this.url = init.query ? `${init.path}?${init.query}` : init.path;
    this.headers = new Headers(init.headers);
    this.searchParams = new URLSearchParams(init.query);
    /** Query parameters as a plain object. */
    this.query = this.searchParams.toObject();
    /** Path parameters such as `:id`, filled in by the router. */
    this.params = {};
    this._body = init.body || "";
    const cookies = {};
    for (const part of (this.headers.get("cookie") || "").split(";")) {
      const eq = part.indexOf("=");
      if (eq > 0) cookies[part.slice(0, eq).trim()] = decode(part.slice(eq + 1).trim());
    }
    this.cookies = cookies;
  }
  async text() {
    return this._body;
  }
  async json() {
    try {
      return JSON.parse(this._body);
    } catch (_) {
      throw new HttpError(400, "the request body is not valid JSON");
    }
  }
  /** A form post (`application/x-www-form-urlencoded`) as a plain object. */
  async form() {
    return new URLSearchParams(this._body).toObject();
  }
  /** JSON or form fields, whichever was sent. */
  async data() {
    const type = this.headers.get("content-type") || "";
    return type.includes("application/json") ? this.json() : this.form();
  }
}

export class Response {
  constructor(body, init = {}) {
    this.status = init.status === undefined ? 200 : init.status;
    this.headers = new Headers(init.headers);
    this.body = body === undefined || body === null ? "" : String(body);
  }
  static json(data, init = {}) {
    return json(data, init.status, init.headers);
  }
}
globalThis.Response = Response;

export function json(data, status = 200, headers) {
  const response = new Response(JSON.stringify(data), { status, headers });
  response.headers.set("content-type", "application/json");
  return response;
}
export function text(body, status = 200, headers) {
  const response = new Response(body, { status, headers });
  if (!response.headers.has("content-type")) response.headers.set("content-type", "text/plain; charset=utf-8");
  return response;
}
export function html(body, status = 200, headers) {
  const response = new Response(body, { status, headers });
  response.headers.set("content-type", "text/html; charset=utf-8");
  return response;
}
export function redirect(location, status = 303) {
  return new Response("", { status, headers: { location } });
}

// ---------------------------------------------------------------- router

function compile(pattern) {
  const names = [];
  const source = pattern
    .split("/")
    .map((part) => {
      if (part === "*") {
        names.push("*");
        return "(.*)";
      }
      if (part.startsWith(":")) {
        names.push(part.slice(1));
        return "([^/]+)";
      }
      return part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    })
    .join("/");
  return { regex: new RegExp(`^${source}/?$`), names };
}

export class Router {
  constructor() {
    this._routes = [];
    this._middleware = [];
  }
  /** Run `fn(request, next)` around every handler; call `next()` to continue. */
  use(fn) {
    this._middleware.push(fn);
    return this;
  }
  on(method, pattern, handler) {
    this._routes.push({ method, handler, ...compile(pattern) });
    return this;
  }
  get(pattern, handler) {
    return this.on("GET", pattern, handler);
  }
  post(pattern, handler) {
    return this.on("POST", pattern, handler);
  }
  put(pattern, handler) {
    return this.on("PUT", pattern, handler);
  }
  patch(pattern, handler) {
    return this.on("PATCH", pattern, handler);
  }
  delete(pattern, handler) {
    return this.on("DELETE", pattern, handler);
  }
  all(pattern, handler) {
    return this.on("*", pattern, handler);
  }
  async fetch(request) {
    const method = request.method === "HEAD" ? "GET" : request.method;
    let pathMatched = false;
    let found = null;
    for (const route of this._routes) {
      const match = route.regex.exec(request.path);
      if (!match) continue;
      pathMatched = true;
      if (route.method !== "*" && route.method !== method) continue;
      const params = {};
      route.names.forEach((name, i) => {
        params[name] = decode(match[i + 1]);
      });
      found = { route, params };
      break;
    }
    const run = async () => {
      if (!found) {
        return pathMatched ? json({ error: "method not allowed" }, 405) : json({ error: "not found" }, 404);
      }
      request.params = found.params;
      return normalise(await found.route.handler(request));
    };
    let index = 0;
    const next = async () => {
      const layer = this._middleware[index++];
      return layer ? layer(request, next) : run();
    };
    return next();
  }
}

// ---------------------------------------------------------------- database

function params(args) {
  return args.length === 1 && Array.isArray(args[0]) ? args[0] : args;
}

/**
 * The site's own SQLite database. Use `?` placeholders; never build SQL from input.
 */
export const db = {
  /** All rows of a SELECT, as objects. */
  async query(sql, ...args) {
    return call("db.query", { sql, params: params(args) });
  },
  /** The first row, or null. */
  async get(sql, ...args) {
    const rows = call("db.query", { sql, params: params(args) });
    return rows.length ? rows[0] : null;
  },
  /** Run an INSERT, UPDATE or DELETE. Returns `{ changes, lastInsertRowid }`. */
  async run(sql, ...args) {
    return call("db.run", { sql, params: params(args) });
  },
  /** Run several statements in one transaction: `[[sql, params], ...]`. All or nothing. */
  async batch(statements) {
    return call("db.batch", {
      statements: statements.map((s) => (Array.isArray(s) ? { sql: s[0], params: s[1] || [] } : { sql: s, params: [] })),
    });
  },
};

// ---------------------------------------------------------------- dispatch

function normalise(result) {
  if (result instanceof Response) return result;
  if (result === undefined || result === null) return new Response("", { status: 204 });
  if (typeof result === "string") return text(result);
  return json(result);
}

/** Used by the worker: run one request through the backend's default export. */
export async function __dispatch(app, raw) {
  let response;
  try {
    const request = new Request(JSON.parse(raw));
    const handler = typeof app === "function" ? app : app && typeof app.fetch === "function" ? (r) => app.fetch(r) : null;
    if (!handler) throw new Error("backend/index.js must export a Router (or a function) as its default export");
    response = normalise(await handler(request));
  } catch (error) {
    if (error instanceof HttpError) {
      response = json({ error: error.message }, error.status);
    } else {
      // Details go to the log, never to the visitor.
      log.error("unhandled error:", error);
      response = json({ error: "internal error" }, 500);
    }
  }
  const status = Number.isInteger(response.status) && response.status >= 200 && response.status <= 599 ? response.status : 500;
  return JSON.stringify({ status, headers: response.headers._list, body: response.body });
}
