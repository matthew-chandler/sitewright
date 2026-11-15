# Pre-publish checklist

Go through this after writing the files and before `publish_site`.

## Structure

- [ ] `index.html` exists at the top level.
- [ ] Every page links back to the home page and shares one navigation.
- [ ] Every internal link and asset path resolves (`validate_site` reports no errors).
- [ ] No page is a dead end or an empty stub.

## Head

- [ ] `<!doctype html>` and `<html lang>`.
- [ ] `<meta charset>`, viewport meta, a unique `<title>`, a `<meta name="description">`.
- [ ] One stylesheet link, with the right relative path from pages in subfolders.
- [ ] The analytics line `<script src="/_sw/a.js" defer></script>` on every page.

## Content

- [ ] One `<h1>` per page; headings do not skip levels.
- [ ] No placeholder text. Anything invented (names, numbers, dates) is noted for the report.
- [ ] Text is specific to the request, not generic filler.

## Design

- [ ] Works at 360px wide: no horizontal scrolling, tap targets at least 44px.
- [ ] Looks right in both light and dark colour schemes.
- [ ] Focus is visible on links and buttons when using the keyboard.
- [ ] Images and SVGs have `alt` text or are marked decorative.

## Weight

- [ ] No external requests.
- [ ] JavaScript only where it earns its place, and the page works without it.
