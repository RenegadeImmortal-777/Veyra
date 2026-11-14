# Veyra Hosted Registry

Veyra 11 supports both the built-in local registry and hosted registries.

## Selecting a hosted registry

Set the environment variable `VEYRA_REGISTRY_URL` to an HTTP(S) URL that returns a Veyra registry index JSON document.

Example metadata:

```json
{
  "registry_version": 1,
  "packages": {
    "example": {
      "version": "1.0.0",
      "entry": "example.vy",
      "url": "https://packages.example.com/example.vy",
      "dependencies": ["another-package"]
    }
  }
}
```

A package may alternatively provide a `files` object for a multi-file package, mapping safe relative paths to download URLs.

## Install flow

`veyra add <package>` resolves the selected registry, installs declared dependencies recursively, downloads the package, updates `veyra.json`, and rewrites `veyra.lock` with content hashes.

Dependency cycles are protected by an installation traversal set, so a cycle does not recurse indefinitely.

Downloaded file paths are validated as relative normal paths. Absolute paths, parent traversal and non-normal path components are rejected.

## Integrity, signing, self-hosted publishing, and federation

Veyra 12 provides a sovereign cryptographic registry workflow, monorepo workspace resolution, and federated cache:

- **Signing keys & signatures**: `veyra pkg keygen [name]`, `veyra pkg sign [dir] [key]`, and `veyra pkg verify [target]` sign and verify package directories and `.vypkg` bundles using HMAC-SHA256 (`veyra.sig.json`) + SHA-256 content hashes.
- **Portable `.vypkg` bundles & semver constraints**: `veyra pkg pack [dir] [out]` and `veyra pkg unpack <bundle> [dest]` package/unpack `veyra.bundle.v1` archives; `veyra add <pkg>@<range>` (including scoped `@org/pkg@^1.2.0`) enforces semver constraints (`^`, `~`, `>=`, `<=`, `>`, `<`, `=`, `*`), and `veyra.lock` format 3 records `sha256`, `signed`, `native`, and `dependencies`.
- **Federated registries & offline cache**: `VEYRA_REGISTRY_URLS` accepts comma- or semicolon-separated HTTP(S) or local registry indexes in priority order with automatic failover; fetched indexes and artifacts are cached under `$VEYRA_HOME/cache/` and can be served offline with `VEYRA_OFFLINE=1`.
- **Workspaces & scoped packages**: `veyra.json` supports `"workspaces": ["packages/*", ...]` for monorepo member resolution (`use "@org/pkg"`, `import_package("@org/pkg")`, `veyra pkg install` from workspace root).
- **Self-hosted registry server**: `veyra pkg serve [dir] [port]` serves `GET /index.json`, `GET /bundles/<pkg>.vypkg`, and `POST /publish` so `VEYRA_REGISTRY_URL=http://127.0.0.1:8787 veyra publish` and `veyra add` work end-to-end over HTTP.
- **Per-platform native plugin packages**: packages can declare `"native": { "library": "<name>", "source": "..." }` or per-target platform maps (`"windows-x86_64"`, `"linux-x86_64"`, `"macos-aarch64"`, etc.) in `veyra.json` (built with `veyra pkg build-plugin`) to download or compile `veyra_plugin_v1` shared library functions directly via `use "<pkg>"`.
- **PyPI / Python bridge scaffolding**: `veyra pkg py-bridge <pypi_pkg> [out_dir] [--pip-install]` generates a sovereign Veyra wrapper package delegating via the in-process CPython C-API bridge (`py_import`, `py_get`, `py_call`, `py_to_tensor`).

`veyra audit` verifies installed package directories against the SHA-256 content hashes (and HMAC signatures when signed) stored in `veyra.lock`.

The local bundled registry remains the default when `VEYRA_REGISTRY_URL` and `VEYRA_REGISTRY_URLS` are not set. Hosted package retrieval requires the native-services build feature.
