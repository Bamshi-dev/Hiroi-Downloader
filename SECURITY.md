# Security policy

Hiroi Downloader handles data from servers that may be hostile: file
names, headers, playlists, web pages. It also handles secrets such as
cookies and passwords. We take reports about either seriously.

## Reporting a vulnerability

Please do not open a public issue. Use the "Report a vulnerability"
button on the repository's Security tab, which opens a private GitHub
security advisory. Include:

- what an attacker can do, and what they need (a malicious server, a
  local account, a crafted link);
- the steps or the input that reproduce it;
- the version or commit you tested.

You will get an answer within a week. Once the problem is fixed and a
fixed version is available, the advisory is published with credit to
you, unless you prefer otherwise.

## What counts

Examples of what we treat as security problems:

- a server can make Hiroi write outside the chosen download folder;
- a cookie, password, or token ends up in a log, an error message, a
  file on disk, or a request to a different host;
- a hostile response crashes Hiroi or makes it use unbounded memory or
  disk space;
- another local user can control the background service;
- Hiroi sends a request outside the configured proxy when a proxy is
  required.

Bugs that only make a download fail are ordinary bugs; please report
them as issues.
