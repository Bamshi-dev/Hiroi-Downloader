# Security

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: the "Report a
vulnerability" button on the repository's Security tab. Do not open a
public issue for them.

A useful report says what an attacker can do and what they need (a
hostile server, a local account, a crafted link), how to reproduce it,
and the version or git revision tested.

## Commands and settings that help

Who sees a download, and what it gives away:

- `hiroi exposure URL` shows every server a download would reach, what
  each one sees, and what the settings below change, without
  downloading anything. `--check-ip` asks which address servers see.

Proxies and VPNs:

- `--proxy URL` sends everything through a proxy; `socks5h://` lets
  the proxy look host names up, so your DNS server does not see them.
  FTP and SFTP go through SOCKS5 proxies only; torrents never run with
  a proxy set.
- `--require-proxy` (or `HIROI_REQUIRE_PROXY=1`) refuses to run
  without a proxy.
- `--interface NAME` binds every connection to one interface, such as
  a VPN's, and downloads stop when it goes down. `--bind-address IP`
  picks the local address. `hiroi network interfaces` lists the
  interfaces.

Where downloads may connect:

- `--ip-filter FILE` never connects to the addresses in an eMule
  `.dat`, PeerGuardian `.p2p`, or CIDR list.
  `hiroi network check-filter FILE` reads one and counts its ranges.
- Remote sites cannot redirect a download into your local network
  (localhost, the router, the LAN); `--allow-local-redirects` turns
  that check off.
- Redirects from HTTPS to plain HTTP are refused;
  `--allow-insecure-redirects` follows them.
- Certificates are checked; `--no-check-certificate` turns that off,
  and should stay off.
- SFTP servers are checked against `~/.ssh/known_hosts`.

What requests carry:

- `--strip-tracking` removes tracking parameters from links.
- `--no-referer` never sends a Referer.
- `--no-cookie-jar` ignores cookies servers set during a download.
- `--anonymous` sends no User-Agent and no Referer, and strips
  tracking parameters.

Every one of these can be kept in the settings file:
`hiroi settings list privacy` and `hiroi settings list network` show
them, `hiroi settings set KEY VALUE` changes one, and
`hiroi search WORDS` finds them by what they do.
