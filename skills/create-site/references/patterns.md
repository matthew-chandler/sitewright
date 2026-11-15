# Common page patterns

Pick the smallest pattern that fits the request.

## Single page

For a profile, an event, a simple business card, a "coming soon" page.

- `index.html` with sections: hero (name and one-line pitch), details, contact.
- Navigation is a short list of in-page anchor links, or nothing at all.

## Landing page plus a few pages

For a small business, a club, a project.

- `index.html`: hero, three or four highlights, a call to action.
- `about.html`, `contact.html`, and one page per main thing offered.
- Contact details as text and `mailto:` / `tel:` links. There is no server to receive a
  form, so do not build a form that posts anywhere; say so if the user asked for one.

## List plus detail pages

For a log, a catalogue, recipes, trails, a portfolio.

- `index.html`: an introduction and the list as cards (title, one line, link).
- One page per item in a folder: `trails/mount-si.html`. Each links back to the list and
  to the previous and next item.
- Keep facts in a consistent block on each detail page (a `<dl>` works well).
- For more than about a dozen items, add a small script that filters the list as the
  visitor types. The unfiltered list must work without JavaScript.

## Writing

- Lead with what the visitor came for. Short paragraphs. Plain words.
- Headings describe their section; they are not slogans.
- Match the tone of the request: a hiking log is personal, a dentist's site is calm and
  factual.
