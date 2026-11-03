#!/usr/bin/env python3
"""Generate pinned, offline Flatpak Cargo sources from Cargo.lock (Python >=3.11)."""
import json
import pathlib
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
sources = []
for package in sorted(lock["package"], key=lambda p: (p["name"], p["version"])):
    source = package.get("source")
    if source is None:
        continue
    if source != "registry+https://github.com/rust-lang/crates.io-index":
        raise SystemExit(f"Unsupported source; add an explicit pinned rule: {source}")
    name, version, checksum = (package[key] for key in ("name", "version", "checksum"))
    destination = f"cargo/vendor/{name}-{version}"
    sources.extend([
        {
            "type": "archive",
            "archive-type": "tar-gzip",
            "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
            "sha256": checksum,
            "dest": destination,
        },
        {
            "type": "inline",
            "dest": destination,
            "dest-filename": ".cargo-checksum.json",
            "contents": json.dumps({"package": checksum, "files": {}}),
        },
    ])
sources.append({
    "type": "inline",
    "dest": "cargo",
    "dest-filename": "config.toml",
    "contents": '[source.crates-io]\nreplace-with = "vendored-sources"\n'
                '[source.vendored-sources]\ndirectory = "/run/build/wireless-display/cargo/vendor"\n',
})
output = ROOT / "flatpak/cargo-sources.json"
output.write_text(json.dumps(sources, indent=2) + "\n")
print(f"Generated {output.name}: {(len(sources) - 1) // 2} pinned crates")
