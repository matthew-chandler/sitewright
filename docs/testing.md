 # Testing

## Automated tests

```sh
cargo nextest run --workspace
```

| Layer | Where | What it covers |
| --- | --- | --- |
| Unit | `src/` of each crate | Name and path validation, config parsing, SSE parsing at every chunk boundary, retry backoff, visitor hashing, site validation rules, sessions, tokens, rate limiting |
| Provider fixtures | `crates/sw-llm/tests` | The adapter against a local mock server: text, tool-call round trips, fragmented arguments, error codes, retries, truncated streams |
| Site store | `crates/sw-sites/tests` | Create, file sandbox, symlink refusal, publish, automatic rollback, pruning, soft delete and purge |
| Agent loop | `crates/sw-agent/tests` | Full create and edit runs with a scripted provider; tool errors; turn, token and spend limits; cancellation; one run per site |
| End to end | `crates/sitewright/tests` | The daemon booted in-process on a temp directory: serving, the CLI over the admin socket, `prompt` against a mock model, and the web UI including passkeys |

The passkey tests use a small software authenticator written in the test
(`crates/sitewright/tests/ui.rs`): it creates an ES256 credential and signs assertions the
way a phone would, so enrollment, sign-in, replay protection and wrong-origin (phishing)
rejection all run in CI without a browser.

Live provider tests are manual: `scripts/dev-secrets.sh local`, start the daemon, and run
`sitewright prompt`.

## Manual checklist: phone flows

Run this on a real phone at the end of M3 and M6. It needs HTTPS on the admin hostname.

Setup, once, on the server:

```sh
sitewright admin invite     # prints a one-time link, valid 15 minutes
```

| # | Step | Expected |
| --- | --- | --- |
| 1 | Open the invite link in Chrome on Android | "Enroll a passkey" page |
| 2 | Enter a name, tap **Create passkey**, confirm with fingerprint or PIN | Lands on **Sites**, signed in |
| 3 | Open the invite link again | "expired or was already used" |
| 4 | Browser menu → **Add to Home screen** / **Install app** | Sitewright icon on the home screen; opens without browser chrome |
| 5 | In the installed app, **Settings → Sign out**, then **Sign in with a passkey** | Signed in again with one biometric prompt, no typing |
| 6 | **New site**: type a prompt; check the suggested name; **Create site** | Run page appears; text and tool calls stream in as they happen |
| 7 | Wait for **succeeded**; tap **View site**, then **Open site** | The new site opens at `<name>.<zone>` |
| 8 | On the site page, **Edit with a prompt** (for example "make the header dark blue") | A new run streams; the live site shows the change |
| 9 | **Releases → Roll back** on the first release, confirm | The live site shows the first version again at once |
| 10 | **Delete → Delete this site**, type a wrong name | Refused: "type … exactly to confirm" |
| 11 | Type the exact name, **Delete site** | Back on **Sites**; the site's address shows a 404 |
| 12 | Ask the agent to delete a site ("delete this site") | The run asks for confirmation; nothing is deleted until the name is typed |
| 13 | **Settings → Models → Test** | "Works" with a latency |
| 14 | Turn on airplane mode and open the app | The shell loads; pages say they cannot reach the server (nothing stale is shown) |
| 15 | Rotate the phone, switch to dark mode | Layout holds; colours follow the system theme |

Record the result (date, phone, browser, any failures) in `docs/STATUS.md`.
