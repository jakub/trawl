# Issue #119 self-hosted font evidence

Captured from release-mode Trunk distributions built at this branch's final
implementation state on 2026-08-19. Issue #118's browser harness is still
absent, so these captures and the request transcript below are the manual
fallback required by #119.

- [`workbench.png`](workbench.png) renders the fleet-ui workbench from its
  built `dist/` directory.
- [`spa-login.png`](spa-login.png) renders the trawl SPA `/login` route from
  `trunk serve --release`.

Chrome requested both fonts while rendering the workbench:

```text
GET /fonts/Geist-Variable.woff2 HTTP/1.1     200
GET /fonts/GeistMono-Variable.woff2 HTTP/1.1 200
```

Direct requests against both built distributions returned the same result:

```text
distribution  path                                  status  content-type  length
workbench     /fonts/Geist-Variable.woff2            200     font/woff2    69664
workbench     /fonts/GeistMono-Variable.woff2        200     font/woff2    71160
SPA           /fonts/Geist-Variable.woff2            200     font/woff2    69664
SPA           /fonts/GeistMono-Variable.woff2        200     font/woff2    71160
```

Recursive scans of both distributions found zero `fonts.googleapis.com` or
`fonts.gstatic.com` references. Each distribution contains the two WOFF2
files, `OFL.txt`, `README.md`, and `SHA256SUMS`; the three checksummed files
match the fleet-ui source package. The trawl-web integration test separately
pins the production response header to `style-src 'self' 'unsafe-inline'` and
`font-src 'self'` and rejects either Google host.

## Release size delta

The same Trunk commands were run against a clean archive of
`origin/main` (`59a50576e63b6da5a13bb616201b404a5198d2ef`) with the same toolchain.
Sums are uncompressed bytes across every file in each `dist/` before generated
gzip/Brotli sidecars:

```text
distribution  origin/main  issue-119  delta
workbench      3,875,222    4,022,166  +146,944
SPA           22,788,949   22,935,839  +146,890
```

The copied fleet-ui font package itself is 146,518 bytes: 140,824 bytes of
WOFF2 font data and 5,694 bytes of license, source, and checksum evidence.
