# Rust review: Google's style guide

Date: November 3, 2026.

Source: [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html).

All 35 Rust files across the four crates were reviewed, including tests. The
focus was correcting verifiable deviations without changing the architecture,
protocol, video modes, or dependencies. The guide includes contextual
recommendations; this review is not automatic certification of full compliance.

## Corrected occurrences

The “Before” column identifies code before the refactoring. Links point to the
current files.

| Location | Before | Refactoring | Reference |
| --- | --- | --- | --- |
| [coordinator.rs](../crates/app/src/coordinator.rs), [mirroring.rs](../crates/app/src/mirroring.rs), [ui.rs](../crates/app/src/ui.rs), [dummy.rs](../crates/core/src/dummy.rs), [capture.rs](../crates/platform/src/capture.rs), [network.rs](../crates/platform/src/network.rs), [gtk_smoke.rs](../crates/app/tests/gtk_smoke.rs), [x11_fallback.rs](../crates/app/tests/x11_fallback.rs) | `use wireless_display_core::*` or an equivalent glob inside an import tree. | Explicit imports; test-only items stay in the test module. | [Glob imports](https://google.github.io/styleguide/rustguide.html#dont-use-glob-imports). |
| Imports across the four crates | `use std::{net::IpAddr, sync::Arc, time::Duration}` and similar trees. | Each declaration imports items from a single module. | [Import organization](https://google.github.io/styleguide/rustguide.html#a-use-statement-should-import-items-from-one-module). |
| [mirroring.rs](../crates/app/src/mirroring.rs), [diagnostics.rs](../crates/app/src/diagnostics.rs), [streaming.rs](../crates/media/src/streaming.rs), [quality.rs](../crates/media/src/quality.rs), [source.rs](../crates/core/src/protocol/wfd/source.rs), [transport.rs](../crates/core/src/protocol/wfd/rtsp/transport.rs) | External calls such as `select_capture(...)`, `probe_encoders(...)`, `element(...)`, `parse(...)`, and `setup_ports(...)`. | Calls qualified by their responsible module. `Write` is also imported locally in formatting functions. | [Functions and types](https://google.github.io/styleguide/rustguide.html#qualify-calls-to-top-level-functions-with-a-module-name). |
| [model.rs](../crates/core/src/model.rs), [source.rs](../crates/core/src/protocol/wfd/source.rs), [pipeline.rs](../crates/media/src/pipeline.rs), [streaming.rs](../crates/media/src/streaming.rs), [network.rs](../crates/platform/src/network.rs) | `new`/`new_streaming` constructors returned `Result`. | Implementation in `try_new`/`try_new_streaming`; internal calls migrated. Previous names remain as documented aliases. | [Initialization names](https://google.github.io/styleguide/rustguide.html#naming-initialization-functions). |
| [ui.rs](../crates/app/src/ui.rs), `refresh_modes` | `modes: &Rc<RefCell<Vec<VideoMode>>>`. | `modes: &RefCell<Vec<VideoMode>>`; the function does not need to know about the `Rc` owner. | [Input references](https://google.github.io/styleguide/rustguide.html#accept-flexible-references-for-immutable-data). |
| [mirroring.rs](../crates/app/src/mirroring.rs), quality response | `quality_target.is_some()` followed by `quality_target.take().expect(...)`. | Pattern matching combines the target and controller without a panic on this read. Taking the target and acknowledging the change remain in the same stage. | [Pattern matching](https://google.github.io/styleguide/rustguide.html#prefer-pattern-matching-over-check-then-unwrap). |
| [dummy.rs](../crates/core/src/dummy.rs), `receiver` | Three `unwrap_or_else(\|_\| unreachable!())` calls on fixed data. | `expect` explains the constant ID, MAC, and name invariants. None of these values come from the TV or user. | [Invariant context](https://google.github.io/styleguide/rustguide.html#prefer-expect-over-unwrap). |
| [source.rs](../crates/core/src/protocol/wfd/source.rs) and [mirroring.rs](../crates/app/src/mirroring.rs) | Literals such as `request(..., false, ...)` and `controller.rejected(true)`. | Argument comments identify `require` and `format_change`, preserving existing signatures. | [Boolean API parameters](https://google.github.io/styleguide/rustguide.html#prefer-enum-over-bool-in-apis). |
| Queries in domain, state, formats, encoders, and environment | Results such as `VideoMode::size()`, `fps()`, `DeviceId::as_str()`, and `RtpSockets::ports()` could be ignored without a specific warning. | `#[must_use]` on queries and the `PreparedFormat` guard. `Result` already provides this protection. | [Using return values](https://google.github.io/styleguide/rustguide.html#must_use-on-functions). |
| [model.rs](../crates/core/src/model.rs), `ModelError`; [dummy.rs](../crates/core/src/dummy.rs), `MockCancelled` | Errors without allocated data implemented only `Debug` and `Error`. | Derived `Clone` and `Copy`, without changing variants or messages. | [Traits](https://google.github.io/styleguide/rustguide.html#specific-guidance-for-important-traits). |
| [traits.rs](../crates/core/src/traits.rs), constructors, and [quality.rs](../crates/media/src/quality.rs) | Ownership, failure, and ordering contracts depended mainly on internal comments or external architecture documentation. | Documented capture leases, discovery/P2P cleanup, construction without starting media, and retaining a frame until the RTSP ACK. | [Documentation](https://google.github.io/styleguide/rustguide.html#when-and-how-to-write-documentation). |
| [capabilities.rs](../crates/core/src/protocol/wfd/capabilities.rs), [streaming.rs](../crates/media/src/streaming.rs), [main.rs](../crates/app/src/main.rs) | Iterator chains constructed directly in `for` headers. | Named iterators before the loop. | [Iteration](https://google.github.io/styleguide/rustguide.html#implementation-concerns). |

## Additional local optimizations

These improvements are compatible with the guide but are not, by themselves,
style violations in the earlier code:

- [Message::serialize](../crates/core/src/protocol/wfd/rtsp/message.rs) used
  `text.push_str(&format!(...))`. `write!` now writes directly to the final
  `Vec<u8>`. This removes temporary strings for each header and Content-Length.
  Parser validation, limits, and injection protection remain.
- [WfdSource::request](../crates/core/src/protocol/wfd/source.rs) recreated
  `cseq.to_string()` while validating the response. It now borrows and compares
  the received header against the already constructed request header.
- [video::codecs](../crates/core/src/protocol/wfd/video.rs) recounted
  `value.split(',').count()` during R2 parsing. A `Peekable` iterator identifies
  the last set without repeating the count. Size, count, and field limits are
  still checked in the same order.

These changes reduce RTSP control and capability-parsing work. No FPS, CPU, or
memory benchmarks were run. Video encoding resolution, bitrate, preset, and
encoder were not changed in this review.

## Contextual decisions retained

- Rules marked `[chrome]` do not apply to this Cargo app. Tokio, `async`/`await`,
  the backend thread, and the four crates remain.
- `Rc` keeps GTK callbacks on the main thread. `Arc` keeps pipelines, FDs, and
  native callbacks alive. Replacing them with references would change lifetimes
  and increase complexity; `refresh_modes` needed only a borrow.
- Remaining globs are GTK/GStreamer preludes, `super::*` in tests, local enum
  variants, or module reexports. These are contextually accepted cases.
- `Vec`, slices, and local tuples remain where consumers already need those
  formats. Modules retain their domain groupings.
- `new` and `new_streaming` are compatibility aliases. `DeviceId` keeps its
  public `impl Into<String>` parameter to avoid breaking consumers.
- Message types are internal to the coordinated workspace; `#[non_exhaustive]`
  was not imposed, and no generic compatibility layer was created.
- Test-assertion `unwrap` calls remain. Network, portal, and GStreamer failures
  still use `Result`; poisoned mutex contents are not recovered.

## Verification

Before changes: formatting, check, and the default suite passed, with 49 executed
tests and nine environment-dependent tests ignored.

After changes:

| Verification | Result |
| --- | --- |
| `cargo fmt --all --check` | Passed. |
| `cargo check --workspace --all-targets --locked` | Passed. |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Passed without warnings. |
| `cargo build --workspace --locked` | Passed. |
| `cargo test --workspace --locked` | 52 tests passed; nine ignored in the default run. |
| Five `on_dbus` tests explicitly run on a private bus | Passed. |
| `gtk_smoke`, `x11_fallback`, `x11_capture`, explicitly run with Xvfb and a private bus | Three passed. |

Total: 60 tests passed across the default suite and integration runs. The three
new tests verify request/response bytes with binary bodies, Content-Length
replacement, injection/duplicates, and message/header limits.

Transport tests use loopback and synthetic media. Real discovery remains
ignored: there was no TV connection or capture of the user's screen. Xvfb was
extracted into `/tmp` without installing system packages. Builds used cached
Rust dependencies and available native libraries with temporary linker aliases
in `/tmp`.

`Cargo.toml`, `Cargo.lock`, dependency versions, and rustfmt configuration remained
unchanged. `unsafe_code = "forbid"` remains enabled across the workspace.
