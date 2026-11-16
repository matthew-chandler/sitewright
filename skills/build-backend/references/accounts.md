# Visitor accounts

Use this when the site needs visitors to sign up and sign in: members-only pages, "my
entries", anything saved per person. Sitewright handles passwords and sessions. Never
write your own password hashing, session tokens or users table.

```js
import { Router, json, auth, db, HttpError } from "sitewright";
```

## The API

| | |
|---|---|
| `await auth.signUp({ email, password })` | Creates the account and signs it in. Returns `{ user, cookie }`. Answers 400 for a bad email or a password under 8 characters, 409 if the email is taken. |
| `await auth.signIn({ email, password })` | Returns `{ user, cookie }`. Answers 401 for a wrong email or password, 429 after five failures in 15 minutes. |
| `await auth.user(req)` | The signed-in user, or `null`. |
| `await auth.requireUser(req)` | The signed-in user, or answers 401. |
| `await auth.signOut(req)` | Ends this session. Returns `{ cookie }`. |
| `await auth.setPassword(user.id, newPassword)` | Changes the password and signs the user out everywhere. Check the old password first with `auth.signIn`. |
| `await auth.deleteUser(user.id)` | Deletes the account. Delete the user's rows in your tables first. |

A `user` is `{ id, email, created_at }`. The 400/401/409/429 answers happen by
themselves (they are thrown as `HttpError` with a message fit to show the visitor); do
not catch them.

**You must send the cookie.** `signUp`, `signIn` and `signOut` return a `cookie` string;
put it in a `set-cookie` header of your response, or nothing changes for the visitor:

```js
app.post("/api/signup", async (req) => {
  const { user, cookie } = await auth.signUp(await req.data());
  return json({ user }, 201, { "set-cookie": cookie });
});
app.post("/api/signin", async (req) => {
  const { user, cookie } = await auth.signIn(await req.data());
  return json({ user }, 200, { "set-cookie": cookie });
});
app.post("/api/signout", async (req) => {
  const { cookie } = await auth.signOut(req);
  return json({ ok: true }, 200, { "set-cookie": cookie });
});
app.get("/api/me", async (req) => json({ user: await auth.user(req) }));
```

The browser then sends the cookie with every request to the site by itself. Pages call
`fetch("/api/me")` to learn who is signed in; they cannot read the cookie, by design.

## Per-user data

Users live in a table Sitewright manages, `sw_users (id, email, created_at)`. Refer to it
from your own tables and always filter by the signed-in user's id:

```sql
-- backend/migrations/0001_create_notes.sql
CREATE TABLE notes (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES sw_users (id),
    body       TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX notes_user ON notes (user_id, id);
```

```js
app.get("/api/notes", async (req) => {
  const user = await auth.requireUser(req);
  return json(await db.query("SELECT id, body FROM notes WHERE user_id = ? ORDER BY id DESC LIMIT 100", user.id));
});
app.delete("/api/notes/:id", async (req) => {
  const user = await auth.requireUser(req);
  // The user_id condition is what stops one visitor deleting another's note.
  const { changes } = await db.run("DELETE FROM notes WHERE id = ? AND user_id = ?", req.params.id, user.id);
  if (!changes) throw new HttpError(404, "No such note.");
});
```

- You can `SELECT id, email, created_at FROM sw_users` and join on it. Name the columns:
  `SELECT *` on it is refused, as is any write. Extra facts about a user (display name,
  role) go in a table of your own keyed by `user_id`.
- Every route that reads or changes personal data starts with `auth.requireUser(req)`
  and uses `user.id` from it, never an id sent by the browser.
- Use `POST`, `PUT` or `DELETE` for anything that changes data, never `GET`. The session
  cookie is not sent with cross-site form posts, which protects those routes.

## Testing

`test_backend` returns the response headers. Sign up, take the `name=value` part of the
`set-cookie` header (up to the first `;`), and pass it as `cookie` in later calls to act
as that visitor. Test at least: a signed-out request is refused, a second account
cannot see the first one's data, and signing out ends access.

## Not available

Email verification, password reset by email, social sign-in and admin roles. There is no
way to send email. Say so in your summary if the request needed one of these, and note
that the owner can remove an account with their own tools if someone is locked out.
