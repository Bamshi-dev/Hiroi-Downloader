# Settings and search

## Where settings live

Settings live in one of two places:

- **The settings file**, `config.toml` in Hiroi's config folder
  (`~/.config/hiroi` on Linux), or the file `HIROI_CONFIG` names. It
  holds the engine's settings, and each has a command-line flag of the
  same name, which wins over the file. The service reads the file when
  it starts.
- **The service**, for what it changes while it runs: notifications,
  smart folders, and the speed limit. These apply at once.

`hiroi settings` treats both alike:

```sh
hiroi settings                       # every setting and its value
hiroi settings list network          # one section: network, privacy, disk, or service
hiroi settings get interface
hiroi settings set interface wg0
hiroi settings set speed-limit 2M
hiroi settings reset interface       # back to the default
hiroi settings path                  # where the file is
```

Values are checked when they are set and when the file is read, so a
misspelled name or a wrong value stops with a message instead of being
ignored. The file can hold a proxy password, so it is readable by its
owner only. It looks like this:

```toml
interface = "wg0"
proxy = "socks5h://127.0.0.1:9050"
strip-tracking = true
```

## Every setting

Network:

- `proxy`: proxy for all requests.
- `per-host`: most connections to one server, across all downloads
  (16).
- `min-speed`: reconnect connections slower than this.
- `interface`: connect only through this interface.
- `bind-address`: connect from this local address.
- `ip-filter`: never connect to the addresses in this file.
- `allow-local-redirects`: let remote sites redirect into the local
  network (off).
- `allow-insecure-redirects`: follow redirects from HTTPS to plain HTTP
  (off).
- `no-check-certificate`: accept certificates that do not check out
  (off).

Privacy:

- `require-proxy`: refuse to run without a proxy (off).
- `strip-tracking`: remove tracking parameters from links (off).
- `no-referer`: never send a Referer (off).
- `no-cookie-jar`: ignore cookies servers set (off).
- `anonymous`: no User-Agent, no Referer, no tracking parameters
  (off).

Disk:

- `no-prealloc`: don't reserve disk space first (off).
- `no-sync`: don't flush to disk before saving progress (off).

Service:

- `notifications`: desktop notifications when downloads finish (on).
- `smart-folders`: move episodes of a series into a folder (on).
- `speed-limit`: combined speed limit of the service (unlimited).

[Network and privacy](network-and-privacy.md) explains the network
and privacy settings in full.

## Search

```sh
hiroi search vpn
hiroi search group pause
hiroi settings search cookie
```

`hiroi search` looks through settings, commands, and options for every
word given, and lists what it finds by kind, names that match first.
Each option is listed once, with the commands that take it.

The browser extension's options page has a search of its own.
