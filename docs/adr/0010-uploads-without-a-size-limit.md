# ADR-0010: Uploads have no size limit

- Status: accepted
- Date: 2026-11-17
- Changes: ADR-0009, points 1 and 5

## Context

Matthew asked for the upload size limit to be removed, with a confirmation popup for
files over 15 MB. Under ADR-0009 uploads were held to the 1 MB per-file limit and photos
were shrunk in the browser to fit it, which rules out video and loses photo quality.

Lifting the limit has consequences elsewhere: the request body was read into memory
(the dev VPS has under 600 MB free), every release directory was a full copy of the
site, the page-weight check would refuse to publish any page showing a large upload,
and the deployment sits behind nginx (`client_max_body_size 25m`) and Cloudflare (100 MB
per request on the free plan).

## Decision

1. **No size limit on uploads.** `max_file_bytes`, `max_site_bytes` and the page weight
   budget apply to what the agent writes, not to `uploads/`. A page whose uploads take
   it over the budget gets a validation *warning* telling the agent to lazy-load.
2. **The browser asks before sending anything over 15 MB** (`window.confirm`, naming the
   files and sizes). Declining skips those files and uploads the rest.
3. **Bodies are streamed to disk** into `<data>/tmp/` and renamed into the working tree.
   The file being replaced is set aside by rename, not read. Measured locally: a 400 MB
   upload with publish kept the daemon's anonymous memory near 7 MB (libgit2 maps the
   file while hashing, which shows as reclaimable file-backed memory).
4. **The browser sends files in 8 MB parts** to `/sites/<name>/uploads/parts/<id>`,
   then finalizes with `PUT /sites/<name>/uploads?name=…&upload=<id>`. No request
   approaches a proxy's limit, progress can be shown, and a dropped connection resends
   one part. The id is random from the client; a part at the wrong offset gets 409 with
   the server's length. Parts left behind are purged after a day. The API and CLI keep
   the single streamed `PUT`.
5. **Releases share unchanged files.** A new release directory hard-links every file
   whose blob is the same as in the newest existing release, instead of copying it.
   Release directories are never written in place, so sharing inodes is safe. The
   working tree is never linked to (the agent's writes truncate in place).
6. **Exports stream files**, and git blobs are streamed out of the object store when
   it allows, so nothing reads a whole upload into memory.
7. **Photos are no longer shrunk.** Only HEIC is converted (to WebP or JPEG, at most
   4096 px), because browsers cannot display it.

## Consequences

- An upload costs about three times its size on disk: the working tree, the git
  object, and one copy shared by all releases (measured: 400 MB upload, three releases,
  382 MiB each for the tree, `.git` and `releases/`). History keeps removed and replaced versions. Disk use is the owner's to manage.
- Containerized-site images and exports include uploads, so they grow with them.
- The portal works behind nginx's 25 MB `client_max_body_size` as deployed. Only API
  uploads through a proxy are bound by its limit; the example config and
  `vps-production.sh` now use `0` and `proxy_request_buffering off`.
