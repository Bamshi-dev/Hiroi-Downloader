# Network and privacy

## Proxies

```sh
hiroi get URL --proxy socks5h://127.0.0.1:9050
hiroi get URL --proxy http://proxy.example:3128
```

- `socks5h://` lets the proxy look host names up, so your DNS server
  does not see them; `socks5://` looks them up on this machine.
- A login goes in the URL: `socks5h://user:password@host:1080`.
- HTTP and HTTPS go through any proxy. FTP and SFTP go through SOCKS5
  proxies (the control connection and every data connection) and
  refuse to run with an HTTP proxy rather than connect around it.
- BitTorrent refuses to run with a proxy set: peers and the DHT cannot
  be kept inside one.
- `--require-proxy`, or `HIROI_REQUIRE_PROXY=1` in the environment,
  refuses to run without a proxy, so a forgotten setting cannot expose
  your address.

## Binding to a VPN

```sh
hiroi network interfaces                 # lists them; likely VPNs are marked
hiroi get URL --interface wg0
hiroi settings set interface wg0         # for every download from now on
```

With `--interface`, every connection leaves through that interface,
and fails when it is gone. Bound to a VPN's interface, a dropped VPN
stops downloads instead of letting them continue over your normal
connection. On Linux, sockets are bound to the device itself;
elsewhere, to the interface's address, looked up for each download.

`--bind-address IP` picks the local address connections start from;
with `--interface`, it must be one of that interface's addresses.

Host names are still looked up by the system's resolver, which may
not use the VPN. A `socks5h://` proxy through the VPN avoids that.

## The IP filter

```sh
hiroi network check-filter blocklist.p2p       # reads it and counts the ranges
hiroi get URL --ip-filter blocklist.p2p
```

Hiroi never connects to the addresses in the filter, whatever name led
there. It reads eMule `.dat` files (entries with a level of 128 or more
are allowed, as in eMule), PeerGuardian `.p2p` files, and plain lists
with one range (`first - last`), CIDR block (`10.0.0.0/8`), or address
per line. IPv4 and IPv6 both work; `#` and `//` start comments. A file
that blocks nothing is refused, so a broken file is not mistaken for
an empty one.

With a proxy that looks names up itself, only the addresses written
in links can be checked.

## Redirects

- Remote sites cannot send a download into your local network
  (localhost, the router, and other machines on the LAN) through a
  redirect or a playlist. Hosts you named, and every host of a
  download that started on the local network, are exempt.
  `--allow-local-redirects` turns this off.
- Redirects from HTTPS to plain HTTP are refused, since they expose
  the download to tampering; `--allow-insecure-redirects` follows
  them.

A refused address drops that source and says why; with mirrors, the
others carry on.

## Certificates

Certificates are checked against the system's trust store, for HTTPS
and FTPS. `--no-check-certificate` accepts any certificate: anyone on
the way can then read and change the download. It exists for servers
you control and cannot fix.

SFTP servers are checked against `~/.ssh/known_hosts`.

## What requests carry

- `--strip-tracking` removes tracking parameters (`utm_source`,
  `fbclid`, and the like) from links.
- `--no-referer` never sends a Referer, not even one the browser
  handed over.
- `--no-cookie-jar` ignores cookies servers set during a download.
- `--anonymous` sends no User-Agent and no Referer, and strips
  tracking parameters. A User-Agent you set on purpose is still sent.

Cookies and credentials are never written to disk or to a log.

## What a download gives away

```sh
hiroi exposure https://example.com/file.iso
hiroi exposure URL --proxy socks5h://127.0.0.1:9050 --check-ip
hiroi exposure URL --check-ip --direct
```

`hiroi exposure` follows a download's redirects without downloading
it and shows every server it would reach: the address, whether the
connection is encrypted, the content network serving it, the cookies
each one sets, and tracking parameters. It then says what each of the
settings above changes. `--check-ip` asks an address-echo service
which address servers see (that contacts the service), and `--direct`
asks once more without the proxy to compare.

What servers do with what they see (keeping logs or sharing them)
cannot be seen from here.
