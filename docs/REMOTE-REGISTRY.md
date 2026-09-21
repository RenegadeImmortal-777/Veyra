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

## Integrity

`veyra audit` verifies installed package directories against the SHA-256 content hashes stored in `veyra.lock`.

The local bundled registry remains the default when `VEYRA_REGISTRY_URL` is not set. Hosted package retrieval requires the native-services build feature.
