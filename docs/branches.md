# Branches

A branch is a place you can edit in parallel with main and merge back. Four
things make one: a git worktree, a wsync daemon on that worktree, a Studio open
on a copy of the place, and a journal of what that Studio changed. Nothing
else. Everything below exists to keep it that small.

## Principle: track, don't diff

The plugin sees every change the moment it happens. So a branch does not export
its place and have a merge reconstruct what happened; it records what happened.
A merge is replaying that record into main's Studio, where main's plugin has its
own record since the same moment. Two journals, one numbering of instances,
replay with three conflict rules. No snapshots, no assembler on the merge path,
no similarity heuristics.

Scripts are not in the journal. wsync writes every script to disk as it
changes, so a branch's script work is its worktree and `branch finish` merges
it with git. The Studio replay skips `Source` on any file-backed script, so git
and the replay never fight over one byte.

## Lifecycle

```
wsync branch new <name>        worktree + daemon + clone + Studio; checkpoint
wsync branch ls                every branch, its Studio, its daemon, its owner
wsync branch up <name>         reopen a closed branch: clone, renumber, replay
wsync branch rm <name>         close the Studio and daemon; keep everything
wsync branch rm <name> --purge also the worktree, the clone and the journal
wsync branch finish <name>     git merge, Studio replay into main, close
```

`branch new`:

1. `git worktree add .wsync-branches/<name> -b wsync/<name>` from main's HEAD.
2. Copy the place out of main's Studio: one `copy` per service (the clipboard
   path that already exists), assembled into `<name>.rbxl` beside the worktree
   with rbx_binary. Studio migrates a place as it opens, so the checkpoint is
   taken from the branch Studio after it loads, never from this file.
3. Write the branch's project file: main's tree, `initialSync: "disk"`, and
   `placeIds: [<synthetic id>]` (see Discovery).
4. Start a daemon on the worktree (`wsync daemon start --project`), launch
   Studio on the clone, keep the pid.
5. When the plugin connects, both plugins take the checkpoint: walk the tree in
   `GetChildren` order and number every instance `<n>`. The trees are
   identical, so the numbering is identical. Main's plugin records the
   checkpoint under the branch name; the branch plugin starts its journal.

## Discovery

A branch Studio must find the daemon on its worktree, not main's, and the
plugin ranks daemons by `gameId`/`placeId`. Branches get a synthetic `placeId`
(a hash of the name): the plugin calls `SetPlaceId` on the clone at boot and the
branch's project file claims that id, so main's daemon ranks weak and the
branch's ranks exact. Main's Studio is untouched. A clone that finds no daemon
claiming its id refuses to connect rather than falling through to main.

The clone carries one marker, a `StringValue` under `ServerStorage` with the
branch name, so the plugin knows it is a branch before discovery runs. It is the
only instance the plugin adds to a place and it is excluded from the journal.

## Sync on a branch

wsync's connect rule is Studio-first: disk-only files are swept into a
24-hour backlog. That rule deletes a branch's work the second time it connects.
Branch project files carry `initialSync: "disk"`, a new project field with three
values:

- `studio` (default, unchanged): today's behaviour.
- `disk`: the existing disk-wins path that full-scope projects already offer.
- `refuse`: answer 409 naming the divergent paths; nothing moves.

## Journal

Per Studio, per checkpoint, an append-only stream the plugin pushes to its
daemon in batches, keyed by the checkpoint numbering:

```
{ id, op: add | remove | move | rename | set | attr | tag, ... }
```

Adds carry the subtree as an rbxm blob, serialized on a timer for anything
still in the tree, so a crash loses seconds. Events are coalesced per id: ten
sets of one property are one event. The daemon writes the journal under
`.wsync-branches/<name>/journal/`. A branch is fully described by its clone plus
its journal: `branch up` reopens the clone, renumbers, and replays.

Main keeps one journal with a checkpoint mark per branch. A branch's merge reads
main's events after its mark.

## Merge in Studio

`branch finish`:

1. `git merge --no-ff --no-commit` main into the branch worktree. Conflicts are
   resolved there, with the branch Studio showing the result.
2. Ship the branch journal and blobs to main's daemon. Main's plugin replays
   them against the live tree inside one `ChangeHistoryService` recording:
   - an id only the branch touched: apply;
   - an id only main touched: keep;
   - an id both touched: a conflict, unless the values are equal;
   - remove on one side and any touch on the other: a conflict;
   - move on both sides to different parents: a conflict.
   Conflicts come back as a list. `branch pick <n> ours|theirs` decides;
   `branch finish` again applies the rest.
3. Pause main's daemon, fast-forward main's checkout to the branch tip, resume.
   Scripts and instances go live in the same second.
4. Stop the branch daemon and Studio. The worktree and journal stay until
   `--purge`.

An id that never existed at the checkpoint (added on the branch) cannot be
"recovered" as a rename of something else, because it has no counterpart to be
paired with. That is the whole reason to track instead of diff.

## Locks and budget

One owner per branch, recorded in the worktree, taken by the command that
creates it and stolen only with `--steal`. One merge into main at a time. A
budget on live branch Studios, default 3, because past Roblox's window cap a
launch succeeds into a windowless stub.

## Not in this design

- Merging instances through files. There is no base snapshot, no ours/theirs
  export, no three-way tree merge.
- Rewriting the clone on autosave. The clone is immutable; the journal is the
  state.
- Any change to the sync engine beyond the `initialSync` switch.
