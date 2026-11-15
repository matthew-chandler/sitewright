---
name: create-site
description: Build a new static website from a description - plan the pages, write fast, accessible, mobile-first HTML and CSS with no external dependencies, validate, and publish. Use when the site does not exist yet or has no files.
license: MIT OR Apache-2.0
metadata:
  author: sitewright
  version: "1.0"
---

# Create a site

Build the whole site in one go, then publish it. Do not ask questions first: make sensible
choices, and say afterwards what you assumed.

## Steps

1. **Create it.** If the site does not exist yet, call `create_site` with the name you were
   given and a short human title (for example "Washington Hikes").
2. **Plan briefly.** Decide the pages and what each is for. One or two sentences to the
   user is enough. Prefer a few good pages over many thin ones; a single page is fine for
   a simple request.
3. **Write the files** with `write_file`, in this order:
   - one shared stylesheet (`css/site.css`),
   - `index.html`,
   - the other pages.
   Start from `assets/page.html` and `assets/site.css` (read them with `read_skill_file`)
   unless the request calls for something very different.
4. **Validate.** Call `validate_site`. Fix every error. Fix warnings unless there is a
   reason not to.
5. **Publish.** Call `publish_site` with a one-line message such as "Initial site".
6. **Report.** Tell the user what you built, what you assumed, and the URL.

## What every page needs

- `<!doctype html>`, `<html lang="…">`, `<meta charset="utf-8">`,
  `<meta name="viewport" content="width=device-width, initial-scale=1">`, a `<title>` that
  names the page and the site, and a `<meta name="description">`.
- One `<h1>`. Headings in order. Landmarks: `<header>`, `<nav>`, `<main>`, `<footer>`.
- The same navigation on every page, with relative links (`about.html`, `../index.html`).
- Real content. Write the actual text the site needs; never "Lorem ipsum" or
  "Your content here". If facts are unknown (opening hours, prices), write plausible
  placeholder content and list it in your report so the user can correct it.

## Design rules

- **Mobile first.** Design for a 360px-wide screen, then add `min-width` media queries.
  Nothing may scroll sideways.
- **No external resources.** No CDNs, web fonts, remote images, analytics or embeds unless
  the user asks. Use a system font stack. Draw graphics as inline SVG or CSS.
- **Light and dark.** Use CSS custom properties for colours and provide a
  `prefers-color-scheme: dark` variant.
- **Readable.** Body text at least 1rem, line height about 1.6, lines no longer than about
  70 characters, contrast of at least 4.5:1.
- **Accessible.** `alt` text on images (empty `alt=""` for decoration), labels on form
  fields, visible focus styles, links that say where they go.
- **Small.** Plain HTML and CSS first. Add JavaScript only where it gives the visitor
  something (filtering a list, a menu toggle), in one small file, and make the page work
  without it.

See `references/checklist.md` for a pre-publish checklist and `references/patterns.md` for
common page patterns (landing page, list plus detail pages, single-page profile).

## Limits to remember

- Paths: letters, digits, `-`, `_`, `.`; forward slashes; a normal web extension.
- 1 MB per file, 20 MB per site, about 2 MB per page including its assets.
- You cannot fetch images or data from the web. Do not link to images you have not written.
