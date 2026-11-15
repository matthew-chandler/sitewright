# LLM providers

Sitewright talks to models through its own small `LlmProvider` trait. Each configured
provider needs an endpoint, a model name, an auth style and (usually) an API key. Keys
never go in `config.toml`: they live in `secrets.toml` and are referred to by name.

| Kind | Covers | Status |
| --- | --- | --- |
| `openai_compat` | OpenAI, Azure OpenAI (v1 endpoint), OpenRouter, Groq, DeepSeek, Ollama, vLLM, LM Studio | available |
| `anthropic` | Claude models (Messages API) | arrives in M4 |
| `gemini` | Gemini models (generateContent) | arrives in M4 |

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
| `kind` | required | `openai_compat`, `anthropic` or `gemini`. |
| `base_url` | required for `openai_compat` | API base; `/chat/completions` is appended. |
| `model` | required | Model name (Azure: deployment name). |
| `auth` | `"bearer"` | `"bearer"` sends `Authorization: Bearer <key>`; `"api_key_header"` sends `api-key: <key>`. |
| `key_ref` | none | Name of the key in `secrets.toml`. Omit for endpoints without a key. |
| `timeout_secs` | `300` | Wait for the response to start, and between streamed chunks. |
| `max_retries` | `3` | Retries after a rate-limit, server or network error. |
| `max_output_tokens` | none | Sent as `max_completion_tokens` when set. |
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

# Ollama on the same machine (no key; best effort)
[[llm.providers]]
id = "local"
kind = "openai_compat"
base_url = "http://127.0.0.1:11434/v1"
model = "MODEL_NAME"
```

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
