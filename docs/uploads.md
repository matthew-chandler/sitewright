# Your own files

A site can use files you upload: a logo, photos, a PDF menu, a font, a CSV of opening
hours. The agent cannot fetch anything from the web, so this is how real pictures get
into a site.

## Uploading

On the site's page, under **Your files**, pick one or more files and press **Upload**.
Each file goes into the site's `uploads/` folder with a cleaned-up name (`My Logo.PNG`
becomes `uploads/my-logo.png`) and is published at once. It is then at
`https://<site>/uploads/<name>`. Uploading a file with the same name replaces it.

Then say what to do with it in a prompt: "Use uploads/logo.png in the header" or "Add a
gallery of the photos in uploads". **Use** next to a file puts its path into the prompt
box.

- **Types:** images (PNG, JPEG, GIF, WebP, AVIF, SVG, ICO), fonts (WOFF, WOFF2, TTF,
  OTF), PDF, MP3, MP4, WebM, and text data (TXT, CSV, JSON, Markdown). Not HTML or
  JavaScript: pages and scripts are the agent's.
- **Size:** no limit. Files over 15 MB need a confirmation, because they make pages
  slow to load, especially on phones. The browser sends files in 8 MB parts with
  progress shown, so proxy request limits (nginx, Cloudflare's 100 MB) do not get in
  the way, and a dropped connection only resends one part. Only the single-request API
  is subject to a proxy's limit; the example nginx config lifts it. iPhone HEIC photos, which browsers cannot show, are converted to WebP (or
  JPEG) before they are sent; everything else is stored exactly as uploaded.
- **Disk:** an upload is stored in the site's working copy, its history and its
  releases, so it takes about three times its size on disk (releases share one copy
  between them).
  History keeps every version, even after you remove or replace a file.
- **Not live yet?** If an earlier run left changes that were never published, the upload
  waits for the next publish rather than putting half-finished work live. The page says
  so; the next prompt publishes it.

## What the agent does with them

The agent sees the list of uploads with their sizes and, for images, their dimensions.
It cannot see what a picture shows, so say what it is ("uploads/team.jpg is a photo of
the three of us") if the name does not. It can read text files such as a CSV, but it
cannot change or delete anything in `uploads/`.

## Removing

**Remove** next to a file takes it off the site. If a page still uses it, the file is kept
and you are told which page; ask the agent to stop using it first. Uploads are part of the
site's history like every other file, so rolling back to a release from before an upload
removes it from the live site too.

## From the command line or the API

```sh
sitewright site upload <site> logo.png photos/*.jpg
sitewright site uploads <site>
sitewright site remove-upload <site> logo.png
```

The API has `GET /api/v1/sites/<site>/uploads`, `PUT /api/v1/sites/<site>/uploads?name=<file name>`
with the file as the body (streamed to disk, no size limit), and
`DELETE /api/v1/sites/<site>/uploads/<file>`. The CLI streams files as they are.
