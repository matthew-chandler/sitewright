---
name: edit-site
description: Change an existing website - read the relevant files first, make the smallest edit that does what was asked, keep the existing style, validate, and publish with a clear message. Use for any request to modify, fix, extend or restyle a site that already has files, and for undoing a change.
license: MIT OR Apache-2.0
metadata:
  author: sitewright
  version: "1.0"
---

# Edit a site

The site already exists and someone cares about how it looks. Change what was asked and
nothing else.

## Steps

1. **Look before you edit.** The file list is in your instructions. Use `read_file` on
   every file you are going to change, and on the stylesheet if the change is visual.
   Never rewrite a file you have not read: you would silently throw away its content.
2. **Decide the smallest change.** Which files really have to change? A colour change is
   usually one or two lines of CSS. A new page is one new file plus a navigation link on
   each existing page.
3. **Write.** `write_file` replaces the whole file, so send the complete new content: the
   original with your change applied, not a fragment and not a summary.
4. **Validate** with `validate_site` and fix any errors you introduced.
5. **Publish** with `publish_site`. The message says what changed, in the imperative:
   "Make the header dark blue", "Add a contact page".
6. **Report** what changed in a sentence or two. Mention anything you chose on the user's
   behalf.

## Keep the site's own style

- Reuse the classes, custom properties, spacing and tone already there. Match the existing
  indentation and naming.
- Do not restyle, reorganise, rename files or "clean up" beyond the request, even if you
  would have built it differently.
- A new page copies the head, header, navigation and footer of an existing page.
- When you add a page or rename one, update the navigation on **every** page.

## Common requests

- **"Change the colour / font / spacing"**: edit the stylesheet. If colours are custom
  properties, change them there and check the dark-mode values too.
- **"Add a section / page"**: follow the structure of the nearest similar section or page.
- **"Fix the typo / wrong fact"**: change exactly that text. Search other pages for the
  same mistake.
- **"Remove X"**: delete the content, then remove links to it. Use `delete_file` for whole
  pages, and fix the navigation.
- **"Use my logo / photo / menu"**: the owner's uploads are listed in your instructions
  under `uploads/`. Link to them by path (`uploads/logo.png`, or `../uploads/logo.png` from
  a page in a subfolder), give images `width` and `height` from the listed dimensions, and
  write `alt` text from the file name and the request. Uploads can be large: lazy-load
  images below the first screen and give videos `preload="none"`. You cannot change
  these files. If
  the file the user means is not listed, say they can upload it on the site's page.
- **"Undo that" / "go back"**: call `rollback_site` with no arguments to return to the
  release before the current one. Do not try to reconstruct the old content by hand.
  Call `site_info` to see the release history if the user wants an older version.
- **"Make it better"** with no specifics: pick the two or three highest-value
  improvements (clarity of the first screen, readability on a phone, missing content),
  make them, and say what you chose. Do not redesign everything.

## Analytics and the dashboard

- Keep the line `<script src="/_sw/a.js" defer></script>` on every page, and add it to new
  pages. It is served by the host; do not create that file.
- If your change adds, removes or renames something visitors interact with or a whole
  section of the site, the dashboard may no longer fit. After publishing, load the
  `design-dashboard` skill and update it. For small edits, leave the dashboard alone.

## When the request does not fit a static site

Sites here are static files. There is no server code, database, login or form handler, and
you cannot fetch anything from the web. If asked for something that needs those, build the
closest static version (for example a `mailto:` link instead of a contact form) and say
plainly what is and is not possible.
