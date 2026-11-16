// A guestbook: list entries, add an entry. Adapt the names to the site.
import { Router, json, db, HttpError } from "sitewright";

const app = new Router();

app.get("/api/entries", async () => {
  return json(await db.query("SELECT id, name, message, created_at FROM entries ORDER BY id DESC LIMIT 100"));
});

app.post("/api/entries", async (req) => {
  const input = await req.data();
  const name = String(input.name ?? "").trim();
  const message = String(input.message ?? "").trim();
  if (!name || name.length > 80) throw new HttpError(400, "Please give a name of up to 80 characters.");
  if (!message || message.length > 2000) throw new HttpError(400, "Please write a message of up to 2000 characters.");

  // At most five entries per visitor per day.
  const visitor = req.headers.get("x-sitewright-visitor") ?? "";
  const since = Date.now() - 24 * 60 * 60 * 1000;
  const recent = await db.get("SELECT count(*) AS n FROM entries WHERE visitor = ? AND created_at > ?", visitor, since);
  if (recent.n >= 5) throw new HttpError(429, "That is enough for today. Thank you!");

  const { lastInsertRowid } = await db.run(
    "INSERT INTO entries (name, message, visitor, created_at) VALUES (?, ?, ?, ?)",
    name, message, visitor, Date.now(),
  );
  return json({ id: lastInsertRowid }, 201);
});

export default app;
