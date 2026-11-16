# Containerized sites

Any site can be packaged as a container image that runs **without Sitewright**: on
another server, a cloud container service, or a laptop. The image carries the site's
files, its backend if it has one, and the settings the backend needs. Secrets are never
in the image; you supply them where it runs, and every package comes with a setup guide
that says exactly which.

## Turning it on

- **For a new site**: tick "Containerize this site" on the new-site form.
- **For an existing site**: "Containerize this site" on the site's page, or
  `sitewright site containerize <name>`.

Turning it on changes nothing about how the site runs here. It adds a **Container**
page for the site with three downloads, and shows the setup guide. `--off` (or "Stop
containerizing") removes the page again.

## What you download

| | |
|---|---|
| **Image** (`<site>-image.tar`) | Load with `docker load -i` or `podman load -i`. Built on request from the site's live release, for the processor type of this server. About 25 MB plus the site's files. |
| **Setup guide** (`<site>-SETUP.md`) | Written for this site as it is now: every environment variable it needs, the `docker run` command, how to bring the data along. Also inside the image as `/SETUP.md`. |
| **Data** (`<site>-data.db`) | A copy of the site's database at that moment, if it has one. Includes visitor accounts. Optional; keep it private. |

From the command line: `sitewright site image <name> -o <dir>` writes all three.

The guide is generated fresh each time, because what a site needs changes when you add
a secret, approve a host or allow a model. After changing the site, download a new
image and replace the container; keep its data volume.

## How it runs on its own

Sitewright builds the image itself; no Docker is needed on this server. The image has
two layers on an empty base:

1. `/sitewright`: the same static program that runs this server, about 20 MB. Its
   `run-site` mode serves exactly one site and nothing else: no portal, no agent, no
   other sites.
2. `/site` (the release's files), `/site.json` (the manifest), `/SETUP.md`, and an empty
   `/data` owned by the unprivileged user the container runs as (65532).

`run-site` answers requests the same way the daemon does for that site: static files,
and for `/api` the backend in its jailed worker (ADR-0006), with the site's own database
under `/data`. Migrations are applied at start-up, after a copy of the database is
taken, just as when publishing.

What the daemon normally holds for a site comes from two other places:

| Under Sitewright | In a container |
|---|---|
| Secrets stored on the site's page | Environment variables of the same name. Only the names listed in the manifest can be read by the backend. |
| Approved outside hosts | The manifest: the list as it was when the image was built. |
| AI models, through the daemon's providers | The manifest names each allowed model and where it is reached; the key is `LLM_<MODEL>_API_KEY`. `LLM_<MODEL>_BASE_URL` and `LLM_<MODEL>_MODEL` override the packaged values. The site's backend cannot read these variables. |
| AI limits and usage | The same limits, from the manifest, counted in a small database under `/data`. |
| Visitor accounts | Unchanged: they live in the site's database. |

The manifest is plain JSON without secret values. It does contain the base URL of each
allowed model's endpoint, which for some providers (Azure) includes your resource name.

## What a container does not have

- The portal, the agent, dashboards and visitor statistics.
- HTTPS. It listens on plain HTTP (port 8080 by default); put a proxy with certificates
  in front.
- A way to change settings. Approved hosts, AI limits and the list of secrets are fixed
  at build time.

## Limits and honest caveats

- The image is for the architecture of the server that built it (x86_64 or aarch64).
- The worker's sandbox uses seccomp. Docker's and Podman's default profiles allow a
  process to add its own filter; a custom profile that forbids `seccomp`/`prctl` would
  stop backends from starting, and the site's start-up log says so.
- Running the *live* site inside a container on the Sitewright server itself is not
  what this feature does. Containerizing prepares a site to leave; here it keeps being
  served by the daemon.
