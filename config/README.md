# Choose a configuration

The server and browser proxy share a file. The CLI uses a different schema.

| File | Purpose and reader |
| --- | --- |
| [trawld.toml](trawld.toml) | Minimal standalone starter, read by `trawld` and `trawl-web` |
| [trawld.reference.toml](trawld.reference.toml) | Annotated optional daemon settings and examples; copy the settings you need into the starter |
| [Debian configuration](../crates/trawl-server/debian/trawld.toml) | Deployment settings installed by `trawl-server` at `/etc/trawl/trawld.toml`; read by both packaged services |
| [Helm values](../chart/trawl/values.yaml) | Kubernetes deployment settings; the chart generates the shared server/proxy configuration |
| [Vector fragments](vector/debian/) | Collector configuration, read by Vector; see the [collector guide](https://trawl.sh/getting-started/vector-integration/) |

For a first look, `trawl trial up` runs a disposable trial in Docker on Linux
and writes its own configuration. Follow
[Your first query](https://trawl.sh/getting-started/first-query/). The
standalone starter is for an installation whose storage, databases, and
browser origin you select.

Before starting the standalone daemons:

1. [Provision the two databases](https://trawl.sh/operate/deployment/#provision-the-databases).
   Set `FLEET_DATABASE_URL` for the Fleet keystore and `TRAWL_DATABASE_URL`
   for Trawl app state. They override `[auth].database_url` and
   `[storage].database_url`, respectively. The starter has no database credentials.
2. Choose a writable `[data].path`. Protect a config file that contains
   database credentials so only the service account can read it.
3. [Configure TLS](https://trawl.sh/operate/access/#configure-tls) for the
   HTTPS API. The certificate must cover the hostname in `[web].upstream_url`
   and in the CLI/collector URL. The proxy verifies the API certificate.
4. Set `[web].public_origins` to the exact HTTPS origin served by your
   reverse proxy. Create a private 32-byte random file for
   `[web].cookie_secret_path`, or use the documented
   [session-key environment setting](https://trawl.sh/reference/configuration/#web).
5. [Create roles and keys](https://trawl.sh/operate/access/#create-roles-and-keys).
   People sign in with personal keys; collectors use service keys.

Pass the same file with `trawld --config PATH` and `trawl-web --config PATH`.
Without `--config`, both look for `~/.trawl/trawld.toml`. The API is
`https://localhost:5514`; the local browser proxy listens on port 8090.
For the starter's HTTPS setup, open the origin you configured in the reverse
proxy, not its internal HTTP listener.

The CLI reads `~/.config/trawl/config.toml`, with `[server].url`, token
settings, and optional profiles. Use `trawl --config PATH` to select another
CLI file. See [Connect to a server](https://trawl.sh/start/connect/) and the
[configuration reference](https://trawl.sh/reference/configuration/) for
environment overrides and complete defaults.
