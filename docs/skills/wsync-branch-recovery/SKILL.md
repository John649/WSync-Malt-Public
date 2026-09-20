---
name: wsync-branch-recovery
description: Recover a `wsync branch finish` that failed or left main half-merged — expired journal blobs, a dead branch daemon, "plugin disconnected" mid-replay, a git merge left pending, main's sync paused. Diagnose from the daemon and the branch directory, take the rung that fits, and carry instances over by hand with wsync copy/paste only as the last rung. Use whenever a branch merge errors, hangs, or leaves state behind.
---

# wsync branch recovery

A branch is a git worktree, a daemon on that worktree, a Studio on an
immutable clone, and a journal of what that Studio changed. `finish` merges
the worktree with git and replays the journal into main's Studio. When it
breaks, the state you need is all on disk or one HTTP call away; nothing here
requires guessing.

`finish` already does the first rung itself: it restarts a dead branch
daemon, asks a live branch Studio to serialize an expired subtree again,
replays in bounded chunks, refuses to commit a replay that stopped early or
dropped a subtree, and resumes a merge it left in progress. **So rung 1 is
always "run it again".** Climb only as far as the symptom sends you.

## Ground rules

- Never use File > Save or File > Publish in a branch (clone) window. The
  clone carries the real place ids; saving it overwrites the live place.
- Never `git add -A` or `git commit -a` in the main checkout while a merge is
  in progress. `finish` scopes its own commit; sweeping would commit the
  `.wsync-branches/` directory.
- Do not "refresh a snapshot" by deleting and re-inserting an instance in the
  branch Studio. That journals a `remove` and an `add` under a new id; the
  old id's `add` is still in the journal. Use the recapture op instead
  (rung 3).
- Main's sync is paused on purpose while a merge is parked on conflicts. Only
  resume it when you are abandoning or finishing the merge.
- Two rungs that fail for the same reason → stop climbing, write down the
  state (rung 0) and report it. Do not improvise a third fix on top.

## Rung 0 — read the state (always first)

Run from the main checkout. `<name>` is the branch, `<root>` is
`.wsync-branches/<name>`.

```bash
wsync branch ls --project . --raw
```

```bash
cat .wsync-branches/<name>/branch.json
```

`branch.json` names the `checkpoint`, the `daemonPort`, the `studioPid`, the
`worktree` and the `clone`. Then:

| Question | How to answer it |
| --- | --- |
| Is the branch daemon up? | `curl -s http://localhost:<daemonPort>/hello` — its `branch` field must equal `<name>` |
| Is the branch Studio connected? | `wsync ping --project <worktree>/<project>.json` (an ok ping means the plugin is there) |
| What does the journal hold? | `curl -s "http://localhost:<daemonPort>/journal?checkpoint=<checkpoint>"` → `events`, `blobsOnDisk`, `blobsMissing` |
| What does the daemon owe? | `curl -s -X POST http://localhost:<daemonPort>/journal/pull` → runs one pull pass and answers `pending: [{id, owner, error}]` |
| Blob index on disk | `cat <root>/journal/blobs.json`; the bytes are `<root>/journal/blobs/<id>` |
| Branch daemon log | `wsync daemon logs --project <worktree>/<project>.json --lines 200` |
| Is main mid-merge? | `git -C . rev-parse -q --verify MERGE_HEAD` (prints a hash when a merge is in progress) and `git -C . status --short` |
| Is main's sync paused? | `curl -s http://localhost:<mainPort>/sync` → `{paused: true|false}` (main's port is in `wsync daemon status --project . --raw`) |
| Parked conflicts / decisions | `cat <root>/conflicts.json` and `cat <root>/decisions.json` |

On Windows use PowerShell's `Invoke-RestMethod` in place of `curl`:
`Invoke-RestMethod -Method Post http://localhost:<port>/journal/pull`.

Write the answers down before touching anything. Every rung below names the
answer that sends you to it.

Also confirm both halves are current builds: `wsync --version` for the CLI
and `wsync context --project . --raw` (its `plugin.version`) for the plugin
connected to main; run the same against the worktree project for the branch
Studio. The recapture op, the chunked
replay and the daemon's `/journal/pull` all arrived together; an older CLI or
plugin on one machine turns rung 1 back into the failure this skill exists
for.

## Rung 1 — run finish again

```bash
wsync branch finish <name> --project .
```

This alone clears: a branch daemon that stopped answering (started again), a
blob the plugin still holds but the daemon never pulled (pulled on
reconnect), a blob nobody holds while the branch Studio is open (serialized
again from the live tree), a replay cut off mid-way (the plugin's ledger skips
what already landed), and a merge parked on conflicts you have since decided
with `wsync branch pick`.

Read the error if it fails again. The rest of this skill is indexed by it.

## Rung 2 — "Studio is closed and N subtree(s) never reached the journal on disk"

The branch Studio is closed, and `blobsMissing` names ids the daemon has no
bytes for. Reopen the branch: it restarts the daemon and Studio, replays the
branch's own journal into the clone, and from then on the instances are back
in a live tree that `finish` can serialize again.

```bash
wsync branch up <name> --project .
```

Wait for it to print `Branch <name> is up`, then rung 1. If `up` itself
reports that a subtree's `add` was skipped because its blob is gone, that
instance no longer exists anywhere except in your memory of it — rung 5.

## Rung 3 — "could not be drained for the replay" with the branch Studio open

`finish` already tried the recapture and it did not help. Two possible
reasons, and the message says which:

- "**is no longer in the branch Studio's tree**": the instance was removed
  on the branch after it was added. If the journal also carries a `remove`
  for that id, the `add` is moot and nothing is lost — rerun with
  `--allow-missing-blobs`. If it does not, someone deleted it outside the
  recorder's sight; rung 5 if you want it back.
- "**serializing … again failed**": the plugin is an older build without
  `journal_recapture`, or SerializationService refused the subtree. Do the
  recapture by hand from the branch Studio, then rung 1:

```bash
wsync copy <StudioPath/Of/The/Instance> --project <worktree>/<project>.json
```

```bash
wsync paste --project . --to <Parent/Path/In/Main>
```

That pastes the instance into main outside the journal. The branch's `add`
for it is then a duplicate waiting to happen, so finish with the skip:

```bash
wsync branch finish <name> --allow-missing-blobs --project .
```

## Rung 4 — "plugin disconnected" or "stopped early" during the replay, git merge pending

Main's Studio dropped off mid-replay (older builds) or the plugin caught an
error partway. Nothing was committed. Check main's Studio is still open and
its plugin connected (`wsync ping --project .`), then rung 1: the replay's
ledger skips every event that already landed.

If main's Studio was **restarted** in between, the ledger is gone with it.
Before rerunning, look for instances the first attempt already created — the
branch's added roots, by name, under their parents — and delete the ones that
are there, or the rerun will create them a second time:

```bash
wsync eval --project . --source 'return #game.StarterGui:GetChildren()'
```

(adjust the path; `wsync find` and `wsync ls` list by path too). Then rung 1.

## Rung 5 — the bytes are gone and the instance is gone

Nothing in wsync holds the subtree any more. What remains:

1. The worktree: any script under the lost subtree is on disk in
   `<worktree>`, because scripts sync to files as they change. `git -C
   <worktree> log --stat` shows them.
2. The clone `<root>/<name>.rbxl` holds the tree as it was when the branch
   was cut — useful when the lost instance was a modification of something
   that already existed.
3. Studio's own undo history in the branch window, if it is still open.

Rebuild it in main by hand from those, then finish with
`--allow-missing-blobs` so the merge commits without it.

## Rung 6 — git side

- Conflicted files: resolve them **in the branch worktree**
  (`<worktree>`), stage nothing by hand in main, then rung 1.
- To abandon the merge entirely: `git -C . merge --abort`, then
  `curl -X POST http://localhost:<mainPort>/sync/resume`. Only do this if the
  replay has not landed anything, or main keeps instances whose scripts it
  does not have.
- A merge someone committed by hand is fine: `finish` sees no `MERGE_HEAD`
  and nothing staged, replays, and reports "already in place".

## Rung 6b — the branch Studio is open but will not reconnect

Symptom: `wsync ping` against the worktree project says no plugin, yet the
clone window is open. Older plugin builds stop after a daemon restart with
"The daemon restarted — reconnect to resync" in the plugin's Connect page;
current builds reconnect on their own within about five seconds. Either click
Connect in the clone's WSync panel, or close it and `wsync branch up <name>`,
which restarts both halves.

## Rung 7 — the branch daemon will not start

`finish` and `up` spawn it detached. To run one where you can read it:

```bash
wsync daemon start --project <worktree>/<project>.json --raw
```

It serves in the foreground and prints a readiness line with its port; put
the port into `branch.json` as `daemonPort` if it changed. A port already
serving another project is an error, not a retry. The daemon scans
`7978–7990`; a clone binds only to the daemon whose `/hello` names its branch,
so a wrong port cannot make it attach to main.

## After the merge lands

```bash
wsync branch rm <name> --project .
```

`--purge` also removes the worktree, the clone and the journal. If main's sync
is still paused (`/sync` says so), `POST /sync/resume`. Save main through
Studio's own File menu; wsync never saves a place.

## What to report when you stop

The rung you stopped on, the exact error text, `branch.json`, the `journal`
answer's `blobsMissing`, the `/journal/pull` answer's `pending`, whether
`MERGE_HEAD` exists in main, and the last 50 lines of the branch daemon log.
