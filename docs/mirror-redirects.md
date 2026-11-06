# Mirror redirects

Mirror HTTP clients follow at most five redirects. Each target must have the
original request's host, a subdomain of it (e.g. `flibusta.is` ->
`static.flibusta.is`), or an exact host in `MIRROR_REDIRECT_ALLOWED_HOSTS`.
The optional environment variable is a comma-separated list of hosts (not URLs,
ports, or wildcard patterns), for example `cdn.example.org,archive.internal`.
IPv6 literals may use brackets. The default allowlist is empty.

Loopback, private/unique-local, link-local, unspecified and multicast IP literals
(including IPv4-mapped IPv6 addresses) are rejected even when they match the
original host, unless explicitly allowlisted. IPv4 broadcast is also rejected.
Rejected redirects fail the mirror attempt rather than returning the 3xx body.

The initial configured source URL is not restricted. Internal sources such as
`http://books_archive:8080` continue to work, including same-host redirects. If
an internal source redirects to a private IP literal or another hostname, add
that trusted destination explicitly to the allowlist. Other service clients
(book_library and converter) are unchanged.

This policy checks URL hosts and literal addresses only. It does not prevent DNS
names resolving to private addresses or DNS rebinding; enforce network-level
egress restrictions where that protection is required.
