# rusty-cat 0.3.6 release notes

Release date: 2026-08-25

`0.3.6` is a corrective release for Windows stable-Rust compatibility and
local-file consistency. Applications using `rusty-cat 0.3.5` on Windows should
upgrade; switching the application or workspace to nightly Rust is not
required.

## Windows stable-Rust regression fixed

`0.3.5` called the unstable Windows metadata methods
`volume_serial_number()` and `file_index()`. A downstream Windows build on
stable Rust therefore failed with `E0658` (`windows_by_handle`). This was a
library implementation regression, not a problem with the consumer's task
builder or transfer API usage.

`0.3.6` removes those unstable calls and the crate's direct `windows-sys`
dependency. Checkpoint replacement now uses stable `std::fs::rename`, while
cross-process target ownership uses the existing cross-platform `fs2`
abstraction and one locked target handle. The crate itself does not require
nightly Rust, handwritten FFI, or a Windows-only dependency for this behavior.

This statement concerns `rusty-cat`'s direct dependencies and source. Cargo may
still select platform implementation crates transitively through Tokio,
reqwest, `fs2`, or another dependency; `0.3.6` does not promise that the entire
resolved dependency graph contains no platform-specific crates.

### Published 0.3.5 provenance

The downloaded crates.io `rusty-cat-0.3.5.crate` has SHA-256
`6d4536fb203d11a826ebd5aefb6089121fb13e61f8fd7aa9d53c70c6889383ec` and
was recorded by crates.io at `2026-08-23T02:00:30.674940Z` with no declared
`rust_version`. Its `.cargo_vcs_info.json` names commit
`2ad5bf4b63abca4d0346ee8af084466e30cb342b`, which is not reachable in the
available clone. Tag `0.3.5` points to
`8ab825242d30c508b8980c98d9380ca0206e199d`.

The included source, documentation, and examples in the published crate match
the tag's included files. The only `Cargo.toml.orig` difference is the version:
the tag says `0.2.4`, while the package says `0.3.5`. This evidence shows the
package came from another or now-unreachable VCS state, or that its version was
rewritten before publication; it does not prove which operation occurred. The
fix baseline is the actual published `0.3.5` source (which matches the tag's
source), with the corrective version advanced to `0.3.6`.

## Rust and platform support

The package now declares `rust-version = "1.89"`; Rust 1.89 stable is the MSRV.

The following rows are required publication gates. They do not claim that the
current working tree has already produced those results; native Windows,
mobile-target, full CI, and packaged-consumer evidence remains required before
publication.

| Support level | Targets | Required gate for this release |
|---|---|---|
| Native release gate | GitHub-hosted Linux, macOS, and Windows x64 MSVC | Core library tests on Rust 1.89 and current stable, including applicable native rename, locking, hardlink, and process-exit cases. |
| Compile gate; runtime experimental | `aarch64-linux-android`, `aarch64-apple-ios` | Library cross-compilation. No simulator/device filesystem or crash-recovery claim yet. |
| Best effort | Other targets, architectures, and filesystems | No release gate; consumers must validate their deployment environment. |

Android and iOS compile checks do not establish runtime support for file
locking, rename durability, sync behavior, or crash recovery. Those guarantees
will require emulator/simulator or device filesystem tests before the support
level is raised.

## Content identity replaces physical-file identity

The correctness contract is based on byte content rather than a physical file
ID. This is the portable content-validation behavior available from stable Rust
across the target set:

- when the platform/filesystem permits an active path replacement, a different
  file containing identical bytes can pass content validation;
- changing or replacing it with different bytes is rejected, including when
  length and timestamps are unchanged;
- metadata remains a fast change detector, not proof of content identity;
- the crate does not promise to distinguish two physical files with identical
  contents.

File-backed uploads retain the existing protocol MD5 value. The initial scan
now also creates whole-file and fixed-block SHA-256 identities in the same pass.
Actual part reads are checked against those blocks, and completion performs a
content validation. This prevents a file-signature/content split when a source
is rewritten or replaced between enqueue, part reads, and completion.

Downloads in serial and parallel mode record SHA-256 part digests in an
adjacent `.rusty-cat/<sha256-of-file-name>.rcdl` private namespace.
Cross-process reuse is generation-bound: semantic URL,
stable effective range-request context, total, chunk grid, and a freshly
observed strong ETag must match, and every stored local part digest must
validate. All remote/request binding material is persisted only as a
domain-separated SHA-256 digest. A legacy partial file, missing sidecar,
or sidecar without that proof is not trusted by length—even at full remote
length—and is fetched again from byte zero.

If HEAD prepared a strong ETag, every 206 must return that same value. If HEAD
was skipped or returned no strong validator, an old sidecar is not reused. A
multi-range serial or parallel run must then latch a strong ETag from its first
206 and require it on every later response; a one-range run may complete without
an ETag because it cannot mix generations across responses. Before `Complete`,
every committed range is validated through the currently visible target path.
The sidecar is removed only after that validation succeeds.

The former `<target>.rcdl` location is never read, overwritten, migrated, or
deleted: it may be ordinary user data or a separate transfer target. A
file/symlink at `.rusty-cat`, or a non-empty unmarked directory there, fails
closed without modification. The target lease covers both the visible target
and its hashed sidecar path. Every `.rusty-cat` path component is reserved even
before a marker exists; a visible target inside it, including through a symlink
alias, is rejected before creation.

Custom `BreakpointDownload` implementations must opt into cross-process reuse
with a complete `resume_identity()` context; the safe default is `None`.
Externally injected HTTP clients also disable old-sidecar reuse because hidden
default headers cannot be canonicalized.

This is an intentional compatibility clarification: an identical-content path
replacement can complete where the operating system allows the replacement,
while a same-length different-content replacement fails instead of being
accepted on metadata alone.

## Target ownership and lock boundary

A download first acquires its normalized path lease, then opens and exclusively
locks the actual target. Serial and parallel writes, data syncs, and checkpoint
barriers reuse that one handle. Cooperating processes, including a process
opening a hardlink alias, therefore cannot acquire a second transfer lock. The
OS releases the lock when the handle closes or the owner process exits.

On Windows, the active download target is intentionally opened without delete
sharing. Windows therefore rejects deleting, renaming, or replacing that path
while the task owns the handle, including an identical-content replacement.
This stricter active-transfer behavior prevents the visible path from being
swapped behind the locked handle. After the task reaches a terminal state and
releases the handle, the application may replace the path normally. The
content-identity rule above does not override this operating-system sharing
boundary.

This is cooperative filesystem locking, not a security primitive. Software
that ignores advisory locks may still mutate a file on platforms that allow it,
and some network or unusual filesystems may not implement the required lock
semantics. `rusty-cat` reports lock acquisition failure instead of silently
falling back to path-only ownership, but it cannot guarantee exclusion from an
uncooperative external writer.

## Checkpoint and power-loss boundary

For a download checkpoint, `0.3.6` orders persistence as follows:

1. sync the completed target ranges through the locked target handle;
2. create a unique sidecar temporary file without overwriting another file;
3. write and `sync_all` the complete next snapshot;
4. close the temporary file;
5. rename it over the hashed sidecar in the adjacent `.rusty-cat` namespace;
6. sync the parent directory on Unix.

Part bits remain pending until the target data barrier succeeds. Recovery only
trusts a complete sidecar whose header and stored part digests validate, so it
can recover the last complete old or new logical snapshot; data not represented
by a committed snapshot is safely downloaded again.

This is not a universal sudden-power-loss guarantee. In particular, stable
`std::fs::rename` is not documented as a Windows write-through rename, the
non-Unix path does not sync a directory handle, and storage controllers,
network filesystems, and mount options can weaken persistence. Do not treat
`.rcdl` as an application transaction log or assume that the newest checkpoint
must survive power removal.

## Parallel memory and callback shutdown safety

Parallel part bodies now share one client-scoped byte semaphore. The checked
budget includes each upload part's verification scratch and both explicit
download buffers (response frame plus destination Vec), is 512 MiB on 64-bit
targets and 64 MiB on 32-bit targets, and releases permits through RAII on
success, failure, cancellation, or panic. This replaces a per-file body-only
limit that could underestimate aggregate memory.

Download checkpoint grids retain the existing 1,000,000-part cap on 64-bit
targets. The 32-bit cap is 407,779 parts, derived from a conservative snapshot
peak estimate plus fixed allocation headroom, so digest clone/encode work
cannot independently exceed the 64 MiB mobile policy. Oversized grids fail
before allocating checkpoint tables.

Calling and synchronously awaiting `MeowClient::close()` from a transfer
callback now fails immediately with `InvalidTaskState` before changing client
state. Close must drain and join that callback dispatcher, so waiting on it
inside the callback would otherwise self-deadlock. Return from the callback and
schedule close from another task or ordinary thread.

## Upgrade guidance

Update the dependency and regenerate the consuming application's lockfile:

```toml
[dependencies]
rusty-cat = "0.3.6"
```

No public transfer API migration is required for this fix. If your application
previously depended on distinguishing identical-content files by inode or a
Windows file ID, that behavior is no longer part of the contract; supply an
application-owned immutable file identifier if that distinction is a business
requirement.
