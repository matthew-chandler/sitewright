# Install

Sitewright is one static binary for Linux (x86_64 and aarch64). It needs a server you
control, a domain whose DNS you can edit, and an API key for an LLM provider.

## Quick install

```sh
curl -fsSL https://github.com/matthew-chandler/sitewright/releases/latest/download/install.sh | sudo sh
```

The script (read it first: [`scripts/install.sh`](../scripts/install.sh)):

1. downloads the release for your architecture and checks its SHA-256, and its minisign
   signature if `minisign` is installed;
2. installs the binary to `/opt/sitewright/bin/sitewright`, linked from `/usr/local/bin`;
3. creates the unprivileged `sitewright` user and `/var/lib/sitewright` (mode 0700);
4. installs the hardened systemd unit.

It does not start the daemon and does not touch DNS, firewalls or other web servers.
Running it again upgrades in place.

## Set up

```sh
sudo -u sitewright sitewright init     # asks for your domain and the mode; writes config.toml
```

Choose a mode:

| Mode | Use when | What you provide |
| --- | --- | --- |
| **proxy** | nginx, Caddy or Cloudflare already terminates TLS on this server | A proxy rule forwarding to `127.0.0.1:8080`; examples in [`deploy/`](../deploy) |
| **standalone** | Sitewright is the only web server on the machine | Ports 80 and 443 reachable from the internet |

Then:

1. **DNS.** Point `*.<your domain>` and the admin hostname at the server. See
   [dns-and-tls.md](dns-and-tls.md) for the options and the exact records.
2. **Start it.**
   ```sh
   sudo systemctl enable --now sitewright
   ```
3. **Enroll your passkey.** This prints a one-time link; open it on your phone.
   ```sh
   sudo sitewright admin invite
   ```
4. **Add a model.** In the admin UI: **Settings → Models**. Or in `config.toml`; see
   [providers.md](providers.md).
5. **Check.**
   ```sh
   sudo sitewright doctor
   ```

Now tap **New site**, describe what you want, and watch it build.

## Manual install

Download `sitewright-<target>.tar.gz`, its `.sha256` and its `.minisig` from the
[releases page](https://github.com/matthew-chandler/sitewright/releases), then:

```sh
sha256sum -c sitewright-x86_64-unknown-linux-musl.tar.gz.sha256
minisign -Vm sitewright-x86_64-unknown-linux-musl.tar.gz -P RWTPQ7yYcfU++y7W1z77ZcllL+32prmfxxhMwcPmGzxfGKEfl+faUAYO
tar -xzf sitewright-x86_64-unknown-linux-musl.tar.gz
```

The archive holds the binary and `deploy/sitewright.service`. Any location works; the
daemon needs a data directory it can write (`--data-dir`, default `/var/lib/sitewright`)
and, for `sitewright update`, permission to replace its own binary.

## Running without root or systemd

Everything works as an ordinary user:

```sh
sitewright --data-dir ~/sitewright-data init --zone example.com --mode proxy
sitewright --data-dir ~/sitewright-data serve
```

`deploy/sitewright-dev.service` is a systemd *user* unit for this setup. Set
`SITEWRIGHT_DATA_DIR` in your shell profile to avoid repeating `--data-dir`.

## Build from source

```sh
./scripts/dev-setup.sh
cargo zigbuild --release --target x86_64-unknown-linux-musl -p sitewright
```

See [development.md](development.md).

## Uninstall

```sh
sudo systemctl disable --now sitewright
sudo rm -rf /opt/sitewright /usr/local/bin/sitewright /etc/systemd/system/sitewright.service
sudo rm -rf /var/lib/sitewright      # all sites, history and settings
sudo userdel sitewright
```
