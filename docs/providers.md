# LLM providers

Sitewright talks to models through its own small `LlmProvider` trait. Each configured
provider needs an endpoint, a model name, an auth style and (usually) an API key. Keys
never go in `config.toml`: they live in `secrets.toml` and are referred to by name.

| Kind | Covers | Status |
| --- | --- | --- |
| `openai_compat` | OpenAI, Azure OpenAI (v1 endpoint), OpenRouter, Groq, DeepSeek, Ollama, vLLM, LM Studio | available |
| `anthropic` | Claude models (Messages API) | available |
| `gemini` | Gemini models (generateContent) | available |
| `copilot` | GitHub Copilot subscription models, through GitHub's runtime | experimental, see below |

The agent needs reliable **tool calling** and **streaming**. Small local models are best
effort: they often fail to build a whole site.

## Configuring a provider

`config.toml`:

```toml
[llm]
default = "azure"            # used when a run does not name a provider

[[llm.providers]]
id = "azure"                 # your name for it
kind = "openai_compat"
base_url = "https://RESOURCE.openai.azure.com/openai/v1/"
model = "DEPLOYMENT_NAME"    # for Azure: the deployment name, not the model family
auth = "api_key_header"      # Azure sends the key as `api-key: ...`
key_ref = "azure"            # the key's name in secrets.toml
```

`secrets.toml`, in the data directory, mode 0600 (the daemon refuses to read it if other
users can):

```toml
[keys]
azure = "..."
```

Secrets are re-read on every run, so a new key works without a restart.

### Provider keys

| Key | Default | Meaning |
| --- | --- | --- |
| `id` | required | Name used by `llm.default`, `--provider` and the UI. |
| `kind` | required | `openai_compat`, `anthropic`, `gemini` or `copilot`. |
| `base_url` | required for `openai_compat` | API base. `openai_compat` appends `/chat/completions`. For `anthropic` (default `https://api.anthropic.com`) and `gemini` (default `https://generativelanguage.googleapis.com`) set it only to use a gateway. |
| `model` | required | Model name (Azure: deployment name). |
| `auth` | `"bearer"` | `"bearer"` sends `Authorization: Bearer <key>`; `"api_key_header"` sends `api-key: <key>`. |
| `key_ref` | none | Name of the key in `secrets.toml`. Omit for endpoints without a key. |
| `timeout_secs` | `300` | Wait for the response to start, and between streamed chunks. |
| `max_retries` | `3` | Retries after a rate-limit, server or network error. |
| `max_output_tokens` | none | Upper bound on tokens generated per model call. `anthropic` requires a value and uses 64000 when unset. |
| `extra_body` | `{}` | Extra fields merged into every request, e.g. `{ reasoning_effort = "low" }`. |

### Examples

```toml
# OpenAI
[[llm.providers]]
id = "openai"
kind = "openai_compat"
base_url = "https://api.openai.com/v1"
model = "MODEL_ID"
key_ref = "openai"

# OpenRouter
[[llm.providers]]
id = "openrouter"
kind = "openai_compat"
base_url = "https://openrouter.ai/api/v1"
model = "VENDOR/MODEL_ID"
key_ref = "openrouter"

# Anthropic
[[llm.providers]]
id = "claude"
kind = "anthropic"
model = "MODEL_ID"           # for example claude-sonnet-5-5
key_ref = "anthropic"

# Gemini
[[llm.providers]]
id = "gemini"
kind = "gemini"
model = "MODEL_ID"
key_ref = "gemini"

# Ollama on the same machine (no key; best effort)
[[llm.providers]]
id = "local"
kind = "openai_compat"
base_url = "http://127.0.0.1:11434/v1"
model = "MODEL_NAME"
```

## Notes per provider

**Anthropic.** Requests use the Messages API with `anthropic-version: 2026-10-21` and the
key in `x-api-key`. No sampling parameters are sent. Thinking is left at the model's
default; steer it with `extra_body`, for example
`extra_body = { output_config = { effort = "low" } }`. Claude's turn (including thinking
blocks) is sent back exactly as it was produced, which current models require. Prompt
caching is on, so the growing conversation of an agent run is billed at the cached rate
after the first call.

**Gemini.** Requests use `streamGenerateContent` with the key in the `x-goog-api-key`
header (never in the URL). Tool schemas are reduced to the subset Gemini accepts. Thought
signatures are preserved between turns.

**Any provider.** Use **Settings → Models → Test** (or add the provider and run a prompt)
to check a new provider: the test asks the model to call a tool, which is exactly what the
agent depends on.

## Cost

Token usage is recorded for every run. To turn it into money, add the model's price;
Sitewright ships no prices of its own, because they change.

```toml
[[llm.prices]]
model = "DEPLOYMENT_NAME"    # must equal the provider's `model`
input_per_mtok = 0.25        # US dollars per million input tokens
output_per_mtok = 2.00
```

Without a price for the model in use, a run's cost is unknown and
`agent.daily_spend_cap_usd` cannot apply; the turn and token limits still do.

## Resilience

- Requests that fail with 429, 5xx, a timeout or a network error are retried with
  exponential backoff and jitter. A `Retry-After` header is honoured (capped at 60 s).
- Authentication and bad-request errors are not retried; the run fails with the
  provider's message. API keys never appear in errors, logs or run records.
- A stream that ends before the model finished is an error, not a silent truncation.

## Trying it

```sh
sitewright prompt --new hikes "a hiking log with my favorite Washington trails"
sitewright prompt hikes "add a packing checklist to the home page"
sitewright prompt --provider openrouter hikes "make the headings larger"
```

## GitHub Copilot (experimental)

Copilot is not a plain API. GitHub's supported route is its Copilot runtime, a separate
program of about 210 MB that Sitewright starts for the length of a run and stops
afterwards. Sitewright gives it its own prompt and tools and refuses everything else
the runtime can do (shell, files, web). ADR-0008 has the details and the caveats.

1. Install the runtime on the server (about 150 MB to download, checksum verified):

   ```sh
   sitewright copilot install
   ```

2. Create a fine-grained personal access token on GitHub with the "Copilot Requests"
   permission, then check that everything works. This lists the models your account
   may use and prints how much memory the runtime needed:

   ```sh
   COPILOT_GITHUB_TOKEN=github_pat_... sitewright copilot check
   ```

3. Add the provider: in the portal under Settings, kind "GitHub Copilot", a model id
   from the list, and the token as the key. Or in `config.toml`:

   ```toml
   [[llm.providers]]
   id = "copilot"
   kind = "copilot"
   model = "gpt-5"
   key_ref = "copilot"
   ```

Notes:

- Only one Copilot conversation runs at a time; a second waits.
- Cost is shown as $0. GitHub bills Copilot as premium requests, not per token.
- The runtime's memory use is the open question on a small server. Run the check
  before relying on it.
- `COPILOT_CLI_PATH` points Sitewright at a runtime somewhere else.
- A containerized site cannot use a Copilot model.
