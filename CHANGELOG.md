# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), that adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `wsync branch finish` no longer dies on an `add` whose blob expired out of
  the plugin before the daemon pulled it: the live branch Studio serializes
  the instance again (`journal_recapture`), and the daemon's own pull pass
  does the same. Four causes of the pull never happening are closed: a pull
  pass that unwound left its `pulling` flag set for the daemon's lifetime, a
  restarted daemon had no trigger to pull what it missed (the plugin's
  reconnect and `POST /journal/pull` now are), a flush past the 4096-event
  bound was refused and requeued whole forever (it is posted in pieces), and
  the blob refresh timer re-armed itself every ten seconds.
- The replay into main's Studio goes over in chunks of 250 events and yields
  inside a chunk, so it can no longer outlast the daemon's 8 s heartbeat and
  be cut off mid-recording. A replay that stopped early, or that had to skip
  a subtree it had no bytes for, is not committed: the git merge stays in
  progress and the rerun resumes it. An `add` without its blob is skipped and
  reported instead of being stood in for by an empty instance.
- `finish` starts a branch daemon that is not answering instead of failing on
  the connect, and gives the branch Studio 45 s to find it before deciding the
  Studio is closed.
- A container filled after it was added no longer merges with every child
  twice. The root's refreshed blob already carries the children, and their own
  `add` events now claim those instances instead of creating them again — in
  `finish` and in `branch up`'s self-replay alike.
- A branch clone reconnects on its own after its daemon is replaced. The
  transport fails such a reconnect on purpose ("The daemon restarted —
  reconnect to resync") and the app then only retried under the AutoReconnect
  setting, off by default — so after `finish` restarted a dead daemon the
  clone sat disconnected and the merge read it as a closed Studio.
- An addition from disk adopts an untracked instance already standing at its
  path instead of creating it again beside it. After a replay, the merged
  files' rescan used to create a second copy of every container the replay
  had just put in main, holding only the scripts.
- On macOS, `branch rm` and `finish` now close a branch Studio that ignores
  the polite signal, and `branch new` counts Studio windows by process name
  rather than matching every `StudioMCP` proxy and crash handler as a window.

### Added

- `wsync launch <placefile|placeId>` opens a place in Studio from either a
  `.rbxl` / `.rbxlx` file or a cloud place id (universe from `--universe`, a
  daemon that claims the place, or `--project`'s `gameId`), and only
  launches — no scratch project, no daemon. `wsync open` remains the attach
  form and `wsync studio` the bare launch.
- `wsync branch finish --allow-missing-blobs` commits a merge whose lost
  subtrees were carried over by hand, and the `wsync-branch-recovery` skill
  (`docs/skills/wsync-branch-recovery/SKILL.md`) is the manual ladder for a
  merge the rerun does not clear.

### Changed

- Generated agent instructions now use a concise contract with on-demand
  reference docs, conditional discovery, current backlog semantics, carried
  task authorization, command effects, and explicit context/command budgets.
- Command discovery defaults to a compact offline index; `--full` explicitly
  requests the complete registry. Query output defaults to 200 matches.
- Ordinary CLI commands no longer run installation, update checks, or stats
  persistence as hidden startup/exit work. Use explicit install/update commands.

- Script class and RunContext are now fully encoded in the file suffix — one
  suffix, one meaning, no mode flag: `.server.luau` is a `Script` with
  `RunContext = Legacy`, `.client.luau` a `Script` with `RunContext = Client`,
  `.local.luau` a `LocalScript`, `.runserver.luau` a `Script` with
  `RunContext = Server`, and plain `.luau` a `ModuleScript`
  (`RunContext = Plugin` falls back to `.server.luau` with the value kept in
  the data sidecar). The `legacyScripts` / `emitLegacyScripts` project field is
  parsed but ignored. **Breaking:** existing `.client.luau` files change
  meaning from `LocalScript` to a RunContext-Client `Script` — rename them to
  `.local.luau` (or delete and re-create the project from Studio)
- Code scope now reads and writes the instance-data sidecars
  (`*.meta.json` / `*.data.json`) of in-scope instances, so attributes, tags
  and suffix-inexpressible properties (e.g. `RunContext = Plugin`) round-trip
  instead of re-flagging the connect-time disk review on every reconnect

### Added

- Offline value schemas (`wsync commands types [TypeName]`) and canonical
  Vector3/Color3 examples in real command help. Typed values are validated
  before delivery with actionable field/type errors.
- Eval source-file/stdin input, safe source-file exports, consistent named
  project flags, negative vector support, and documented flag aliases.
- Per-boot operation receipts distinguish not-started, completed, partial,
  and unknown outcomes, including late replies. Project/daemon boot guards
  and pinned playtest job IDs prevent stale-target writes.
- Regression tests parse every documented CLI example and exercise value
  contracts, source input/export, job identity, nil returns, and timeout recovery.

- `AvoidSync` Studio attribute, now implemented: a boolean attribute set to
  `true` on an instance of any class keeps that instance and its subtree in
  Studio — left out of live syncback, the connect-time diff and the
  Studio → disk pull. Setting it on a synced instance removes its files;
  clearing it syncs the subtree back. The daemon does not read it: a sidecar
  carrying `AvoidSync: true` syncs to Studio once and the instance is
  Studio-only from the moment it lands, the file left behind as a stale copy
  the next connect moves to the backlog. Shares the exclusion plumbing with
  `--!ignoresync`, which now also takes effect the moment a file carrying it
  arrives from disk rather than at the script's next Studio edit
- `--!ignoresync` script directive: a script carrying it on its own line within
  its first 10 lines stays in Studio, together with everything under it — the
  plugin leaves it out of live syncback, the connect-time diff and the
  Studio → disk pull, so it is never written to disk and its bytes never count
  against the per-root transfer bound. Adding the directive to a synced script
  removes its file; deleting the directive syncs the script back. Meant for
  scripts that hold bulk data in string literals. The generated agent docs
  describe it next to `AvoidSync`
- Phase 1 engine fork: Argon 2.0.29 core forked as WSync (`argon` → `wsync`, `~/.argon` → `~/.wsync`, `argon.toml` → `wsync.toml`, default port 7978)
- HTTP server rewritten on axum + tokio (long-poll endpoints preserved wire-for-wire)
- `GET /hello` JSON identity endpoint for daemon discovery
- Config keys: `syncback_model_json`, `conflict_engine`, `compat_argon`, `port_scan_max`, `auto_open_app_modal`
- `share_stats` now defaults to `false` and the telemetry upload path is removed entirely (local counters only)

### Fixed

- Batch and playtest source failures now exit nonzero with truthful results;
  fire-and-forget exec reports queue acknowledgement rather than execution.
- Localhost clients bypass system proxy discovery, avoiding the macOS proxy
  initialization path that failed in restricted hosts.
- Relative lint scopes follow an explicitly selected project; absolute paths
  are preserved. Playtest return slots preserve nils and multiple values.

- `wsync stop` resolves its arguments as session ID, then `host:port` address, then bare port — previously only registry IDs matched despite the help text promising address support
- `wsync stop` exits nonzero when nothing was stopped (no matching session, or a managed daemon refused the unauthenticated stop), so scripts can distinguish success from a no-op
- Stopping a session no longer drops registry entries of sessions it never touched: only entries that were actually stopped (or whose process is verified dead) are removed, `wsync stop --all` keeps entries of daemons that refused, and the registry file is now replaced atomically so a concurrent rewrite by a stopping session can no longer be misread as corrupted and wiped
- Session lookups by host/port now require both provided fields to match (previously either one matched, letting shutdown cleanup remove the wrong session's entry)

For engine history prior to the fork, see [Argon's changelog](https://github.com/argon-rbx/argon/blob/main/CHANGELOG.md) up to 2.0.29.
