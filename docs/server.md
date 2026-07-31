# Server modes

`toggl-jira-sync server` exposes the shared app API over HTTP for web and desktop clients.

Local clients also use this server core automatically. The Tauri desktop app starts an embedded single-mode loopback server for the app lifetime, and day-to-day terminal commands such as `tjs`, `tjs tui`, `tjs status`, `tjs sync`, and `tjs schedule ...` start an embedded loopback server for the command lifetime. You only need to run `toggl-jira-sync server` manually when you want a long-lived API process for browser development, integrations, or hosted deployments.

## Single mode

Single mode is the default local mode. It uses the same config, credentials, and Turso-backed local ledger resolution as the CLI, TUI, and desktop GUI.

```sh
toggl-jira-sync server --mode single --host 127.0.0.1 --port 8787
```

Use `--config`, `--credentials`, and `--db` together when running against non-default local files.

Routes are unscoped under `/api`, for example `/api/snapshot`, `/api/config`, `/api/status`, `/api/sync/dry-run`, and `/api/sync`. Keep the default host on `127.0.0.1` unless a reverse proxy or other authentication boundary protects the server.

## Multi mode

Multi mode is a minimal SaaS foundation. It requires a tenant metadata SQLite database and bearer tokens mapped to exactly one tenant.

```sh
toggl-jira-sync server --mode multi --tenant-db ./tenants.sqlite --host 127.0.0.1 --port 8787
```

Each tenant row points to its own config file, credentials file, and sync local database ledger. Tenant API tokens are stored as SHA-256 hashes. Multi mode never uses the default local config or credentials file for tenant requests. HTTP responses expose credential presence only; raw Toggl and Jira credential values are not returned by the server API.

Tenant routes are scoped and require `Authorization: Bearer <token>`:

```text
GET  /api/me
GET  /api/tenants/{tenant_id}/snapshot
GET  /api/tenants/{tenant_id}/config
GET  /api/tenants/{tenant_id}/status
POST /api/tenants/{tenant_id}/sync/dry-run
POST /api/tenants/{tenant_id}/sync
```

A token for tenant `a` cannot access tenant `b`; mismatches return `403`. Missing or invalid tokens return `401`.

For a web client pointed at multi mode, provide the API base URL, tenant id, and token to the frontend build/runtime:

```sh
VITE_TJS_API_BASE_URL=https://tjs.example.com \
VITE_TJS_TENANT_ID=tenant-a \
VITE_TJS_TENANT_TOKEN=replace-with-tenant-token \
bun run build
```

For hosted web origins, allow the exact origin with `TJS_ALLOWED_ORIGINS`:

```sh
TJS_ALLOWED_ORIGINS=https://tjs.example.com toggl-jira-sync server --mode multi --tenant-db ./tenants.sqlite
```

Seed tenants by inserting one row in `server_tenants` and one hashed token row in `tenant_api_tokens`. The token hash format is `sha256:<lowercase-hex-sha256-token>`:

```sql
INSERT INTO server_tenants (tenant_id, slug, display_name, config_path, credentials_path, db_path)
VALUES ('tenant-a', 'tenant-a', 'Tenant A', '/data/tenant-a/config.toml', '/data/tenant-a/credentials.env', '/data/tenant-a/sync.sqlite');

INSERT INTO tenant_api_tokens (tenant_id, token_hash, token_label)
VALUES ('tenant-a', 'sha256:REPLACE_WITH_SHA256_HEX', 'web client');
```

Multi mode intentionally does not include public signup, billing, OAuth, organization management, an admin portal, cross-tenant reporting, tenant config editing, local data deletion, or config export. Add those only after the secret-storage and audit model is designed.

## Deployment guardrails

For Docker or customer-hosted deployments, mount config, credentials, tenant metadata, and local database ledgers as volumes. Do not bake credentials into the image.

SQLite is acceptable for single-tenant and small self-hosted multi-tenant deployments when each tenant has a separate ledger file. If the service becomes high-concurrency or centrally hosted for many customers, move the tenant metadata and sync ledgers to PostgreSQL before adding public SaaS features.
