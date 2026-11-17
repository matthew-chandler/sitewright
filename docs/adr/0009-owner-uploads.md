# ADR-0009: Owner uploads live in the site's git tree under `uploads/`

- Status: accepted; the size limit and in-browser shrinking were replaced by ADR-0010
- Date: 2026-11-16

## Context

Matthew asked for a way to upload his own assets (logos, photos, documents) from the
portal for a site to use. Until now every file in a site was written by the agent as
text, so a site could not show a real photo or logo. The portal is used mostly from a
phone, where a camera photo is 2 to 6 MB and often HEIC, while a site file is limited to
1 MB. The agent cannot see images.

## Decision

1. **Uploads are ordinary site files in `uploads/`** in the site's git working tree.
   They are versioned, published, rolled back, exported and containerized exactly like
   the rest of the site, with the same path rules and size limits. No new storage, table
   or serving path.
2. **Only media, fonts, documents and data can be uploaded** (`UPLOAD_EXTENSIONS` in
   `confine.rs`, intersected with the configured allowlist): never HTML or JavaScript,
   which remain the agent's. File names are cleaned to the path rules
   (`My Logo.PNG` becomes `uploads/my-logo.png`); an upload with the same name replaces
   the file.
3. **An upload publishes at once only when the working tree equals the live release.**
   A run that failed midway leaves unpublished edits in the working tree, and an upload
   must never put those live. When the tree differs (or the site was never published)
   the file waits for the next publish and the owner is told so. If the publish fails,
   the change is undone. Removing an upload follows the same rule, and a page that still
   uses the file makes validation fail, so it is kept.
4. **The agent may read uploads but never write or delete them.** `write_file` and
   `delete_file` refuse `uploads/`. The system prompt lists the uploads with their sizes
   and, for images, their pixel dimensions (read from the file header with the
   `imagesize` crate), so the agent can lay out and describe images it cannot see.
5. **Large photos are made smaller in the browser**, not on the server: `upload.js`
   re-encodes a JPEG, PNG, WebP or HEIC image that is over the limit (HEIC always) to
   WebP, or JPEG where the browser cannot encode WebP, at most 2400 px on its long side
   and stepping down until it fits. The server stores bytes as given.
6. **The portal's thumbnails come from the working tree** through
   `/sites/<name>/uploads/<file>`, served with `Content-Security-Policy: sandbox` so an
   uploaded SVG cannot run script on the admin origin. The admin guard now keeps a CSP a
   handler set instead of overwriting it.

## Consequences

- No image-processing dependency on the server, and no change to the 1 MB file limit.
  Files that are not images (a PDF, a video) must fit the limit as they are; owners can
  raise `[sites] max_file_bytes`.
- Rolling back to a release older than an upload removes it from the live site, as for
  any other file; it is still in the history.
- `imagesize` is a small, dependency-free crate that reads headers only.
- Uploading before a site exists is not possible: the site page is where uploads live.
  The owner creates the site, uploads, then prompts "use uploads/logo.png".
