---
name: setup-domain
description: Help the user point a hostname at this server - explain the DNS options, check what the name does today with check_domain, and give the exact record to add at their DNS host. Use for questions about domains, DNS, subdomains, "my site does not load", custom hostnames, wildcards, Cloudflare or HTTPS certificates.
license: MIT OR Apache-2.0
metadata:
  author: sitewright
  version: "1.0"
---

# Set up a domain

Sitewright never changes DNS. The user adds records by hand at whichever company hosts
their DNS; you tell them exactly what to add and verify the result.

## Steps

1. **Find out where things stand.** Call `site_info` for the site's hostnames, then
   `check_domain` with the hostname in question. It reports the addresses the name
   resolves to and whether a request for it reaches this server.
2. **Interpret the result** (see the table below) and tell the user in one or two
   sentences what is wrong, if anything.
3. **Give the exact record**: type, name, value, and where applicable the proxy setting.
   Use the server's public address if the user has given it; otherwise say "your server's
   public IP address". Never guess an address.
4. **After they add it**, run `check_domain` again. DNS changes can take a few minutes;
   say so rather than concluding it failed.

## Reading `check_domain`

| Result | Meaning | What to tell the user |
| --- | --- | --- |
| Does not resolve | No DNS record matches the name (or it has not propagated) | Add a record (below), wait a few minutes |
| Resolves, reaches this server | Working | Nothing to do |
| Resolves, another server answered | The record points elsewhere, or a proxy in front forwards somewhere else | Fix the record's value, or the proxy's origin |
| Resolves, redirect | Something in front redirects HTTP before it gets here | Usually fine if the redirect goes to HTTPS on the same name; otherwise fix the proxy |
| Resolves, connection failed or timed out | Firewall, or the web server in front is not forwarding this name | Open ports 80/443; check the proxy configuration |

## The three ways to point names here

| Option | Record to add | Good for |
| --- | --- | --- |
| Wildcard on the zone | `*.example.com  A  <server IP>` once | Zero work per site: every new site just works |
| Wildcard one level down | `*.sites.example.com  A  <server IP>` once | Keeping the main namespace untouched |
| Per-site record | `hikes.example.com  A  <server IP>` for each site | No wildcard at all |

Use `AAAA` instead of `A` for an IPv6 address, or add both.

Two DNS rules people trip over:

- **A specific record always wins over the wildcard at that name.** `www.example.com` or
  `mail.example.com` keep working where they point today.
- **Any record at a name stops the wildcard from answering there**, even a lone `TXT` or
  `MX`. If `hikes.example.com` has a TXT record and no A record, the wildcard will not
  apply to it: add an explicit A record.

## A custom hostname for one site

A site can also answer on a name outside the configured zone (for example
`www.janes-bakery.com`). The server owner attaches it to the site; the user then adds a
record for that exact name: `CNAME` to the site's primary hostname, or `A` to the server's
address. At the top of a domain (`janes-bakery.com` itself) most DNS hosts do not allow
`CNAME`: use `A`, or the host's ALIAS/ANAME feature.

## Cloudflare

- **Proxied (orange cloud)** records get Cloudflare's certificate, which on the free plan
  covers only first-level names: `hikes.example.com` yes, `hikes.sites.example.com` no.
  For a wildcard one level down, set the record to **DNS only**, or use a paid certificate.
- Behind Cloudflare's proxy the server should run in proxy mode. Standalone mode (the
  server gets its own Let's Encrypt certificates) needs **DNS only** records so the
  certificate authority can reach the server directly.
- Error 525 or 526 from Cloudflare means it reached the server but could not agree on
  TLS: the web server in front needs a valid certificate for that hostname.

## HTTPS

In proxy mode, whatever sits in front (nginx, Caddy, Cloudflare) holds the certificate;
that is where to look when the padlock is missing. In standalone mode the server requests
a certificate automatically once the name reaches it, which is another reason to get
`check_domain` passing first.
