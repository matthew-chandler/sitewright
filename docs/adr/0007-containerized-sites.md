# ADR-0007: A containerized site is an image of Sitewright's own binary in single-site mode

- Status: accepted
- Date: 2026-11-16

## Context

Matthew asked for sites to be containerizable: chosen when a site is created or later,
with the image downloadable from the portal, and runnable **without Sitewright**. Secrets
may be assumed to be set up where the container runs, and each package must come with a
guide for doing that.

Two facts shape the answer. A site's backend is written against Sitewright's own small
runtime (ADR-0006), not Node or Deno, and depends on the daemon for its database,
secrets, outbound requests and AI models. And neither the dev VPS nor the build machine
has Docker.

## Decision

1. **The image contains the Sitewright binary and runs it as `sitewright run-site`**, a
   mode that serves exactly one site: static files, and the jailed backend worker for
   `/api`. It has no portal, agent, metrics, TLS or other sites. "Without Sitewright"
   therefore means without a Sitewright *server*; the runtime travels in the image.
2. **Sitewright writes the image archive itself** (`sw-image`: the `docker save` format
   with an OCI layout), on request, from the site's live release. No container engine is
   needed on the server.
3. **Settings travel in a manifest; secrets come from the environment.** `site.json`
   lists secret names, approved hosts, and allowed AI models with their endpoints and
   limits. A site secret `NAME` is the variable `NAME`; a model's key is
   `LLM_<MODEL>_API_KEY`. The backend can read only the variables named as its secrets.
4. **Every package has a generated setup guide** (`SETUP.md`), shown in the portal,
   downloadable, and inside the image. It is written from the manifest, so it always
   matches the image.
5. **Data is separate.** The image has code, not rows. A copy of the site's database is
   a separate download, and `sitewright import-data` places it into a volume.
6. **"Containerized" is a per-site flag that enables this packaging.** The site keeps
   being served by the daemon. Running live sites in containers on the Sitewright host
   is out of scope: it needs a container engine there.

## Why

- **The backend API is the runtime.** Translating agent-written handlers to another
  framework at export time would produce code nobody tested. Shipping the runtime they
  were written and tested against means a containerized site behaves the same, down to
  the sandbox, the SQL guards and the visitor accounts.
- **One code path.** `run-site` uses the same responder, worker manager, site database
  and publish-time checks as the daemon. There is no second implementation to drift.
- **Small and dependency-free.** A static binary on an empty base is about 20 MB, has no
  shell or package manager to exploit, and runs as an unprivileged user with a read-only
  root filesystem.
- **No Docker on the server.** Building the archive directly keeps the feature working
  on the hardware Sitewright is meant for, and avoids giving the daemon access to a
  Docker socket (which is root on the host).
- **Secrets by environment** is what every container platform supports, and keeps them
  out of the image, the registry and the download.

## Consequences

- The image is tied to a Sitewright version and to the server's CPU architecture.
- Settings are frozen at build time; changing them means packaging again.
- A site in a container has no agent. Editing it means editing in Sitewright and
  shipping a new image.
- The manifest reveals non-secret configuration, including model endpoint URLs.
- Docker compatibility of the archive is verified in CI (`scripts/container-smoke.sh`
  loads and runs an image), not on the machines Sitewright itself runs on.

## Alternatives considered

- **Generate a Node/Deno project and a Dockerfile**: familiar output, but a different
  runtime from the one the code was tested on, and it needs Docker to build.
- **Static export only** (the existing Docker build context): loses the backend.
- **Run sites in containers on the host**: needs a container engine and a privileged
  socket on the server; not available here and a large new attack surface.
