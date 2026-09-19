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
   The marker is written into this file here, before Studio ever opens it
   (see Identity) — never added to a running place afterwards.
3. Write the branch's project file: main's tree, `initialSync: "keep"`,
   `branch: <name>` — which is what `/hello` reports and what a clone matches
   on — and a name-derived `placeIds` claim. That claim identifies nothing to
   Roblox and is not the clone's place id; it survives only so a branch daemon
   cannot rank exact for main's own window.
4. Start a daemon on the worktree (`wsync daemon start --project`), launch
   Studio on the clone, keep the pid.
5. When the plugin connects, both plugins take the checkpoint: main walks the
   tree in `GetChildren` order and numbers every instance that has no id yet,
   and hands the clone a *shape* that states each node's id. The clone aligns
   its own tree against that shape and takes the ids it names. Main's plugin
   records the checkpoint under the branch name; the branch plugin starts its
   journal.

## Identity: the marker is the branch

A clone boots with the place's **real** `placeId` and **real** `gameId`. It is a
copy of the game, so every Roblox API a script reaches — DataStores,
MessagingService, anything keyed on the place or the universe — behaves in a
clone exactly as it does in main. Nothing about the ids says "branch".

What says "branch" is one instance. The clone carries a `StringValue` named
`WSyncBranch` directly under `ServerStorage` whose `Value` **is** the branch
name. That is the whole identity; there is no second id to keep in step. It is
the only instance wsync adds to a place, and it is excluded from the journal
numbering and from disk sync.

The assembler bakes it into the `.rbxl` before Studio is ever spawned, so the
plugin reads it at boot rather than being told later:

| on `ServerStorage/WSyncBranch` | |
| --- | --- |
| `Value` | the branch name — the identity |
| attribute `GameId` | the real universe id |
| attribute `PlaceId` | the real place id |
| attribute `DaemonPort` | the branch daemon's port |

A place opened from a local `.rbxl` reports `0` for both ids, so the plugin
calls `SetUniverseId` and `SetPlaceId` with the marker's values first, before
discovery runs.

> **A clone window points at the live place.** Because the ids are real,
> Studio's own **File > Save** and **File > Publish** in a clone window would
> overwrite the real place with a half-finished branch. The plugin refuses the
> `save` op on a branch, but Studio's File menu is not reachable from a plugin
> and is **not** covered. `wsync branch finish` is the only way a branch's work
> is meant to reach main.

## Discovery: marker first, refuse rather than guess

Main and every clone now share the same `gameId` and `placeId`, so the ids can
no longer tell them apart and the marker decides first:

- A Studio **carrying** the marker binds only to the daemon whose `/hello`
  `branch` equals the marker's `Value`. Nothing else ranks, at any strength — a
  clone whose own daemon is down refuses to connect rather than falling through
  to main and journalling a branch's edits into the live place. The
  `DaemonPort` attribute is probed first as a shortcut; it is not an override,
  and the daemon it reaches still has to name the right branch.
- A Studio **without** the marker never binds to a branch daemon, however well
  the ids line up. Otherwise main's own window would attach to a branch's
  worktree whenever main's daemon happened not to answer.

Everything else keeps the ordinary strict-`gameId` ranking.

One consequence is worth stating, because it costs an afternoon to rediscover:
a place file carries the universe id it was copied from. A project whose file
claims no `gameId` serves a claimless `/hello`, and under strict-`gameId`
ranking a *published* place — which is what a copied place file still looks
like — never matches a claimless daemon. Such a Studio finds nothing and reports
no broker, and no amount of retrying changes that. If a Studio will not attach,
check that its project file claims the universe id its place file was cut from.

## Sync on a branch

wsync's connect rule is Studio-first: disk-only files are swept into a
24-hour backlog. That rule deletes a branch's work the second time it connects.
Branch project files carry `initialSync: "keep"`, a new project field with four
values:

- `studio` (default, unchanged): today's behaviour.
- `disk`: the existing disk-wins path that full-scope projects already offer.
- `keep`: apply nothing in either direction; start syncing from where both
  sides already are.
- `refuse`: answer 409 naming the divergent paths; nothing moves.

A branch clone takes `keep`, and the reason is worth stating plainly, because
`disk` looks like the obvious answer and is not.

Code scope counts `Folder` as syncable. A place is full of folders that exist
only in the place file — no directory on disk has ever stood behind them — and
the divergence comparison reports every one of them as Studio-only. To a
Studio-first commit that means "write them out"; to a disk-wins commit it means
"delete them". The first clone to connect with `disk` lost 63,000 instances
under `ReplicatedStorage` in one pass.

Neither direction is right here, because the question is wrong. A clone is a
copy of what Studio already holds, taken seconds earlier. The two sides are not
diverged; they are the same tree seen through a projection that only covers
part of it. So the connect reconciles nothing: it writes nothing, removes
nothing, backlogs nothing, and answers
`{ok: true, applied: false, direction: "keep", entries: n}`.

It does seed the conflict baselines from the current tree, which is the one
thing it must do. A baseline is what the engine compares the *next* change
against; without one, the first edit to any instance in the reported set would
look like a change nobody made, and the branch would open on a wall of
manufactured conflicts. Mid-session, `keep` declines to pick a side the same
way `refuse` does: the clash stays parked for `wsync resolve`.

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

## Two or more live branches

`wsync branch new B` while branch A is still open is ordinary, and four
invariants are what make it so. All four exist because the first cut of this
design assumed exactly one branch at a time, and each of them was a way for A's
merge to land on the wrong instances.

**Ids in main are stable for the session, not for a checkpoint.** A checkpoint
numbers only instances that have no id yet, continues the counter, and never
reuses a number. It clears nothing: not the ids, not the `new:` counter, not
the added roots, not the per-checkpoint replay ledgers. A checkpoint that
renumbered main from 1 would silently repoint every id in A's journal at a
different instance, and A's merge would then edit whatever now stood where its
own work used to. Instances a replay creates in main are numbered too — they
are ordinary instances from the moment they land, and a branch cut afterwards
addresses them like any other.

**The checkpoint shape states its ids.** Version 1 of the shape was positional:
`{name, class, childCount}` per node, id = the service's base plus the node's
pre-order index. That only worked while main renumbered from 1. Version 2 adds
the id to each entry, and the clone assigns the id it is given rather than
counting. The alignment itself is unchanged — same two-pointer walk by
`(name, class)`, same `ALIGN_WINDOW`, same `ALIGN_MIN_RATIO`, same accounting
for skipped nodes — so a clone still adopts a tree Studio has added its own
instances to. A v1 shape is still read positionally, so a clone cut against a
main running an older plugin adopts exactly as it did before.

**Ids a clone mints carry its branch name.** Two clones of the same place
number Studio's own migration additions identically, so both would mint `new:1`
for the first instance their user adds, and main would then have one id meaning
two different instances. A clone's ids are `new:<branch>:<m>`; main's own stay
`new:<m>`.

**Each clone binds to its own daemon, by marker.** Three live clones of one
place are three Studios with identical `gameId` and `placeId`; only the marker
separates them, which is why discovery consults it before anything else (see
"Discovery"). Nothing here is quieter than a mis-binding: a clone that attached
to main's daemon would write a branch's tree straight into the live checkout.

**Main records one stream and marks it per branch.** `Journal.start` on a
Studio that is already recording adds the mark and leaves the stream alone.
Restarting it at B's checkpoint sent main's later edits to a stream A does not
read, and A's conflict detection — which is exactly "main's events after A's
mark" — went blind to every edit main made after B was cut. On disk the daemon
fans one `POST /journal/append` out to the stream it names plus every other
checkpoint with a mark in `marks.json`, so each live branch's
`<checkpoint>.jsonl` stays a complete record of main since its own mark. A
branch daemon has no marks file and writes one stream, as before.

`branch finish` and `branch rm --purge` drop their own branch's mark
(`POST /journal/unmark`) and nothing else. No command deletes a checkpoint's
stream: that file is one branch's view of main, and removing one branch must
never take another's with it. A branch that is merely closed keeps its mark,
because `branch up` reopens it and its merge still needs everything main did in
between.

What is still one-at-a-time is the merge, not the branch: `branch finish`
pauses main's sync, merges, replays and commits, and a second `finish` started
underneath it would meet a checkout already mid-merge. The budget on live
branch Studios (default 3) is about Roblox's window cap and nothing else.

### Observed

Three branches live at once on a real place, each on its own daemon: their
clones booted with the place's real `placeId` and `gameId`, each carrying the
marker with its own name and binding to the daemon that named it. A script
edited in one worktree reached that clone's editor source in about six seconds.
One branch was finished while the other two stayed live: the merge took about
nine seconds, main's editor source carried the change, the other two branches
were untouched, and only the finished branch's mark was dropped.

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
