---
name: export-site
description: Export a site so it can be hosted somewhere else - explain the two formats (plain files, or a Docker build context) and build the archive with export_site. Use when the user wants to download, back up, move, migrate or self-host a site elsewhere, or asks for a Docker image or container.
license: MIT OR Apache-2.0
metadata:
  author: sitewright
  version: "1.0"
---

# Export a site

An export is a snapshot of the **live release**: exactly the files visitors see now. It is
built by the server, not by you; your part is to pick the right format and explain what
the user gets.

## Steps

1. **Check it is published.** `site_info` shows whether there is a live release. An
   unpublished site has nothing to export: publish first (or say so).
2. **Pick the format** from what the user wants:

   | They want to... | Format |
   | --- | --- |
   | Upload to another static host (GitHub Pages, Netlify, an S3 bucket, plain nginx), keep a backup, or look at the files | `folder` |
   | Run the site as a container, or deploy to something that builds from a Dockerfile | `context` |

   If it is unclear, use `folder`; it is the simpler one.
3. **Call `export_site`** with the format.
4. **Tell the user** where to get it and what to do with it (below). Do not paste file
   contents into the chat.

## What each format contains

**`folder`**: `<site>-<release>.tar.gz` with one folder holding the site's files. Unpack
and upload the folder's contents; `index.html` is at its top.

**`context`**: `<site>-<release>-docker-context.tar.gz` with:

- `site/`: the files,
- `Dockerfile`: copies them into a small static file server image pinned by digest,
- `README.md`: the two commands needed.

```sh
tar -xzf <file> && cd <site>
docker build -t <site> .
docker run --rm -p 8080:8080 <site>     # then open http://localhost:8080
```

The container serves plain HTTP on port 8080; whatever runs it should terminate TLS.
Docker is not needed on the Sitewright server, only where the image is built.

## Where the download is

- In the admin UI: the site's page, under **Export**.
- On the server: `sitewright export <site> --format folder` (or `context`).

Exports are kept for a day and can be rebuilt at any time.

## Things worth mentioning

- The export is the live release. Unpublished edits are not in it.
- Analytics and the dashboard stay behind: the `/_sw/a.js` line in the pages only works
  on a Sitewright host. Elsewhere it does nothing; offer to remove it with an edit if the
  user is moving away for good.
- The site's history (earlier releases) is not in the export. It stays in Sitewright.
