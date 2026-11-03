# Local Flatpak build

Requires flatpak-builder, configured Flathub, and runtimes/SDKs installed by the user:

```sh
flatpak install flathub org.gnome.Platform//51 org.gnome.Sdk//51 org.freedesktop.Sdk.Extension.rust-stable//26.08
python3 scripts/generate-cargo-sources.py
flatpak-builder --user --install --force-clean build-dir flatpak/org.example.WirelessDisplay.yml
flatpak run org.example.WirelessDisplay --dummy
flatpak run org.example.WirelessDisplay --diagnostics
```

`cargo-sources.json` is generated from Cargo.lock and tracked in version control.
Flatpak-builder downloads SHA-256-verified sources before the build; Cargo builds
with `--locked --offline` and source replacement. Updating Cargo.lock requires
regenerating the JSON. Dependencies from other registries/Git are rejected by
the generator until explicit rules are added.

To separate downloading from the offline build:

```sh
flatpak-builder --download-only --force-clean build-dir flatpak/org.example.WirelessDisplay.yml
flatpak-builder --disable-download --force-clean build-dir flatpak/org.example.WirelessDisplay.yml
```

The manifest uses a local `dir` source for development in this checkout. For
Flathub publication, replace it with a tag/commit or immutable release archive,
and update the provisional ID, homepage, and release metadata. The package is
still experimental.

Structural validation without the SDK:

```sh
flatpak-builder --show-manifest flatpak/org.example.WirelessDisplay.yml
desktop-file-validate data/org.example.WirelessDisplay.desktop
appstreamcli validate --no-net data/org.example.WirelessDisplay.metainfo.xml
```

AppStream reports a missing homepage until the project has a public URL. Do not
use a fictional URL for publication; define the identity/URL before submission.
