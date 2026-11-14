# Kovrek Hosted Registry

Kovrek 11 supports both the built-in local registry and hosted registries.

## Selecting a hosted registry

Set the environment variable `KOVREK_REGISTRY_URL` to an HTTP(S) URL that returns a Kovrek registry index JSON document.

Example metadata:

```json
{
  "registry_version": 1,
  "packages": {
    "example": {
      "version": "1.0.0",
      "entry": "example.kvr",
      "url": "https://packages.example.com/example.kvr",
      "dependencies": ["another-package"]
    }
  }
}
```

A package may alternatively provide a `files` object for a multi-file package, mapping safe relative paths to download URLs.

## Install flow

`kovrek add <package>` resolves the selected registry, installs declared dependencies recursively, downloads the package, updates `kovrek.json`, and rewrites `kovrek.lock` with content hashes.

Dependency cycles are protected by an installation traversal set, so a cycle does not recurse indefinitely.

Downloaded file paths are validated as relative normal paths. Absolute paths, parent traversal and non-normal path components are rejected.

## Integrity, signing, self-hosted publishing, and federation

Kovrek 12 provides a sovereign cryptographic registry workflow, monorepo workspace resolution, and federated cache:

- **Signing keys & signatures**: `kovrek pkg keygen [name]`, `kovrek pkg sign [dir] [key]`, and `kovrek pkg verify [target]` sign and verify package directories and `.kvpkg` bundles using HMAC-SHA256 (`kovrek.sig.json`) + SHA-256 content hashes.
- **Portable `.kvpkg` bundles & semver constraints**: `kovrek pkg pack [dir] [out]` and `kovrek pkg unpack <bundle> [dest]` package/unpack `kovrek.bundle.v1` archives; `kovrek add <pkg>@<range>` (including scoped `@org/pkg@^1.2.0`) enforces semver constraints (`^`, `~`, `>=`, `<=`, `>`, `<`, `=`, `*`), and `kovrek.lock` format 3 records `sha256`, `signed`, `native`, and `dependencies`.
- **Federated registries & offline cache**: `KOVREK_REGISTRY_URLS` accepts comma- or semicolon-separated HTTP(S) or local registry indexes in priority order with automatic failover; fetched indexes and artifacts are cached under `$KOVREK_HOME/cache/` and can be served offline with `KOVREK_OFFLINE=1`.
- **Workspaces & scoped packages**: `kovrek.json` supports `"workspaces": ["packages/*", ...]` for monorepo member resolution (`use "@org/pkg"`, `import_package("@org/pkg")`, `kovrek pkg install` from workspace root).
- **Self-hosted registry server**: `kovrek pkg serve [dir] [port]` serves `GET /index.json`, `GET /bundles/<pkg>.kvpkg`, and `POST /publish` so `KOVREK_REGISTRY_URL=http://127.0.0.1:8787 kovrek publish` and `kovrek add` work end-to-end over HTTP.
- **Per-platform native plugin packages**: packages can declare `"native": { "library": "<name>", "source": "..." }` or per-target platform maps (`"windows-x86_64"`, `"linux-x86_64"`, `"macos-aarch64"`, etc.) in `kovrek.json` (built with `kovrek pkg build-plugin`) to download or compile `kovrek_plugin_v1` shared library functions directly via `use "<pkg>"`.
- **PyPI / Python bridge scaffolding**: `kovrek pkg py-bridge <pypi_pkg> [out_dir] [--pip-install]` generates a sovereign Kovrek wrapper package delegating via the in-process CPython C-API bridge (`py_import`, `py_get`, `py_call`, `py_to_tensor`).

`kovrek audit` verifies installed package directories against the SHA-256 content hashes (and HMAC signatures when signed) stored in `kovrek.lock`.

The local bundled registry remains the default when `KOVREK_REGISTRY_URL` and `KOVREK_REGISTRY_URLS` are not set. Hosted package retrieval requires the native-services build feature.
