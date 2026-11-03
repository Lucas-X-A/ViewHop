# Licenses

Original code in this workspace: MIT, as specified in [LICENSE](LICENSE).
No code from GNOME Network Displays has been incorporated.

Rust dependencies retain their licenses; consult the manifests of the versions
pinned in Cargo.lock (`cargo metadata --format-version=1` includes the `license`
and `license_file` fields). GTK, GLib, GStreamer, and PipeWire are native libraries
with their own terms, usually LGPL; the bundled ximagesrc plugin comes from
gst-plugins-good, LGPL-2.1-or-later. Dynamic linking does not change the license
of our source code. Distributors must preserve notices, provide any required
source code, and check the licenses of the plugins they actually distribute.

No encoder is mandatory. Future codec use may involve distribution requirements
or patents that depend on jurisdiction. Detecting an installed plugin neither
selects a license nor promises WFD compatibility.
