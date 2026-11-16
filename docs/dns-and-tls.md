# DNS and TLS

Sitewright never changes DNS. You add records by hand at whichever company hosts your
domain's DNS; Sitewright tells you whether they work. TLS is either handled by a proxy in
front (proxy mode) or by Sitewright itself with Let's Encrypt (standalone mode).

## Hostnames

- The configuration lists one or more **zones**, for example `example.com`. A site named
  `hikes` is served at `hikes.example.com`. Extra hostnames can be attached to a site.
- The **admin UI** has its own hostname (default `admin.<zone>`). Its first label is
  reserved and cannot be a site name; so are `admin`, `www`, `api` and `mail`.
- A request for any other hostname gets a plain 404, and never a certificate.

## Three ways to point names at the server

| Option | Record | Trade-off |
| --- | --- | --- |
| Wildcard on the zone | `*.example.com  A  <server IP>` once | Zero DNS work per site |
| Wildcard one level down | `*.sites.example.com  A  <server IP>` once (zone `sites.example.com`) | Leaves the main namespace untouched |
| Per-site records | `hikes.example.com  A  <server IP>` for each site | No wildcard at all |

Add an `AAAA` record alongside for IPv6. With a wildcard, also add a record for the admin
hostname if it is not covered by it.

Two DNS rules that surprise people:

- **A specific record always wins over the wildcard at that name.** `www.example.com` and
  `mail.example.com` keep pointing where they point today.
- **Any record at a name stops the wildcard from answering there**, even a lone `TXT` or
  `MX`. If `hikes.example.com` has a TXT record and no A record, the wildcard does not
  apply to it: add an explicit A record.

### Step by step, at any DNS host

1. Find your server's public IP address (`curl -4 ifconfig.me` on the server).
2. In your DNS host's control panel, add a record: type `A`, name `*` (some panels want
   `*.example.com`), value the IP address, TTL automatic.
3. Add a second `A` record for the admin name (`admin`) if you are not using a wildcard
   that covers it.
4. Check it: **Settings → Domains → Check** in the admin UI, or ask the agent
   ("is hikes.example.com set up correctly?"). The check resolves the name and then
   fetches a random token through it, which proves requests really arrive at this server,
   also when a proxy such as Cloudflare sits in between.

## Proxy mode

The daemon listens on plain HTTP (default `127.0.0.1:8080`); something in front
terminates TLS and forwards requests with the original `Host` header.

```toml
[server]
mode = "proxy"
listen = "127.0.0.1:8080"
trusted_proxies = ["127.0.0.1/32"]

[domains]
zones = ["example.com"]
public_scheme = "https"
```

- `trusted_proxies` decides whose `X-Forwarded-For`, `X-Forwarded-Proto` and
  `CF-Connecting-IP` headers are believed. List only your proxy. With an empty list no
  forwarding header is trusted.
- Example configurations: [`deploy/nginx.conf.example`](../deploy/nginx.conf.example) and
  [`deploy/Caddyfile.example`](../deploy/Caddyfile.example). Both forward every hostname
  to the daemon, which routes by `Host`.
- Run-progress pages use server-sent events: the proxy must not buffer responses (the
  examples show how).
- **Give every hostname the same TLS settings.** On nginx, a server block that lacks the
  shared options other blocks include (for example certbot's
  `include /etc/letsencrypt/options-ssl-nginx.conf;`) can fail handshakes from a CDN while
  working from a browser.

A proxy that issues certificates on demand (Caddy) can ask Sitewright whether a hostname
is its own: `GET /.well-known/sitewright/tls-ask?domain=<hostname>` answers 200 for the
admin host and for hostnames that belong to a site, and 404 otherwise.

## Standalone mode

The daemon binds ports 80 and 443 itself (the systemd unit grants
`CAP_NET_BIND_SERVICE`; it never runs as root) and gets certificates from Let's Encrypt.

```toml
[server]
mode = "standalone"
listen = "0.0.0.0:80"

[domains]
zones = ["example.com"]
public_scheme = "https"

[tls]
https_listen = "0.0.0.0:443"
acme_contact = "you@example.com"     # optional
```

How it behaves:

- **Which names get certificates:** the hostnames of your sites and the admin hostname.
  Nothing else, ever. A TLS handshake for an unknown name fails; it never triggers a
  request to the certificate authority, so strangers cannot burn your rate limits by
  asking for random subdomains.
- **When:** shortly after a site or hostname is created (a scan runs every 30 seconds).
  Before asking, Sitewright checks that the name actually reaches it over HTTP, so a name
  whose DNS is not ready is skipped and retried rather than failing validation.
- **Validation:** HTTP-01 on port 80. Plain HTTP is otherwise redirected to HTTPS
  (`tls.redirect_http`).
- **Renewal:** when a third of the certificate's lifetime remains, whatever that lifetime
  is. Every 12 hours Sitewright also asks the certificate authority's renewal information
  (ARI) and renews early if told to.
- **Failures** are retried with growing pauses (1 minute, 2, 4 ... up to 6 hours) and
  shown in **Settings → Certificates** and `sitewright doctor`.
- **Removal:** a deleted site's certificate is deleted too.
- Certificates and keys are in `<data>/certs/<hostname>/` (keys mode 0600); the ACME
  account is in `<data>/acme/account.json`.

For testing, use Let's Encrypt's staging directory, which has generous limits and issues
untrusted certificates:

```toml
[tls]
acme_directory = "https://acme-staging-v02.api.letsencrypt.org/directory"
```

Listen ports are separate settings, so a test instance can serve HTTPS on another port
while a web server in front keeps forwarding port 80 (sites and ACME challenges) to it:

```toml
[server]
mode = "standalone"
listen = "127.0.0.1:18080"

[tls]
https_listen = "0.0.0.0:18443"
redirect_http = false          # keep serving plain HTTP through the proxy
include_admin_host = false     # the proxy handles the admin host
```

## Cloudflare

- **Proxied (orange cloud)** records use Cloudflare's certificate. On the free plan it
  covers first-level names only: `hikes.example.com` yes, `hikes.sites.example.com` no.
  For a wildcard one level down use **DNS only**, or a paid certificate.
- Behind Cloudflare's proxy, run Sitewright in **proxy mode** and add Cloudflare's address
  ranges (or your local proxy) to `trusted_proxies`.
- **Standalone mode needs DNS-only (grey cloud) records**, so Let's Encrypt and visitors
  reach the server directly.
- Error **525** or **526** from Cloudflare means it reached your server but the TLS
  handshake or certificate check failed there: look at the web server in front, not at
  Sitewright. The origin needs a valid certificate for that exact hostname and the same
  TLS settings as your other hostnames.
