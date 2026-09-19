# Addressing a place

WSync targets a daemon by project directory (`--project`) or by port
(`--port`). Neither identifier travels. Porting an asset from one place into
another, driving a place a colleague's daemon serves, or opening something
that exists only in the cloud all need the one name both sides already share:
the **place id**.

Three things are built on it:

| | |
|---|---|
| `wsync places` | what daemons exist and which places they serve |
| `--place <placeId>` | a global selector for every command that talks to a daemon |
| `wsync open <placeId>` | launch Studio on a cloud place, and attach to it |

## `wsync places`

Lists every runtime record under the state directory (`%LOCALAPPDATA%\WSync\daemons\*.json`
on Windows) and probes `GET /hello` on the port each one names. It starts,
stops and writes nothing.

```
PROJECT         PORT  GAME       PLACES                BRANCH    MANAGED BY  PLUGIN             KIND
DungeonDemo     7970  5550001    128374926,128374927   main      cli         WSync (ws)         project
DungeonDemo     7971  5550001    128374926             ui-pass   cli         waiting            project
place-88120033  7973  4410902    88120033              main      cli         WSync (ws)         attach (ephemeral)
```

* **PLACES** comes from the daemon's own `/hello` when it answers, and from
  its project file's `servePlaceIds` when it does not. A record whose daemon
  is gone is still listed, with `PLUGIN` reading `unreachable` — a stale
  record should be visible, not silent.
* **PLUGIN** is `waiting` when the daemon is up but no Studio plugin has the
  bridge, and `<name> (ws|long-poll)` when one does.
* **KIND** marks the scratch projects `wsync open` creates: `attach`, or
  `attach (ephemeral)` when the project deletes itself once Studio closes.

`--raw` prints the same rows as one JSON array.

## `--place <placeId>`

A global option, so it works on every command that binds to a daemon —
`eval`, `exec`, `get`, `set`, `tree`, `lint`, `playtest`, and the rest — with
no per-command flag:

```sh
wsync get --place 128374926 ReplicatedStorage/Shared/Config
wsync tree --place 128374926 --branch ui-pass
```

It resolves against every runtime record on the machine, in this order:

1. keep every daemon whose claim includes the place id — its live `/hello`
   `placeIds`, or, for a daemon that does not answer, its project file's
   `servePlaceIds`;
2. with `--branch <name>`, keep only that branch. `main` and `-` mean the
   branchless daemon. An empty result is an error naming what *was* found,
   never a quiet fall-through to another branch;
3. otherwise a live daemon beats one that only claims the place on paper;
4. then the branchless daemon wins, so a bare `--place` on a place with a
   branch open still means main;
5. anything still tied refuses, and the message lists the candidates.

Nothing claims the place → the error says so and points at
`wsync daemon start --project <path>` and `wsync open <placeId>`.

`--place` is exclusive with `--project` and `--port`: it *is* the target.

## `wsync open`

```sh
wsync open 1234567890 --universe 9876543210   # launch Studio and attach
wsync open 1234567890                         # universe from a daemon that claims it
wsync open --project .                        # this project's first servePlaceIds place
wsync open 1234567890 --no-attach             # launch Studio, nothing else
wsync open ReplicatedStorage/Client/UI        # the original form: select paths
```

A single all-digit argument is a place id; anything else is a Studio path, so
the two forms cannot collide.

Studio is launched through the protocol URL

```
roblox-studio:1+launchmode:edit+task:EditPlace+placeId:<placeId>+universeId:<gameId>
```

which is what makes a *cloud* place openable — no file on disk is involved.
Studio resolves a place inside its universe, so both ids are required. The
universe comes from `--universe`, else from a daemon that already claims the
place, else from `--project`'s `gameId`; with none of those the command
refuses and says which flag to pass.

### Attach mode

If a daemon already serves the place, `wsync open` only launches Studio and
names that daemon.

If nothing serves it, and `--no-attach` was not given, it creates a scratch
project so the place is addressable the moment Studio connects:

* at `<projectsRoot>/_places/<placeId>/`, where `projectsRoot` comes from the
  desktop app's `%APPDATA%\WSync\state.json`; without that file, at
  `~/.wsync/places/<placeId>/`. `--into <dir>` overrides both;
* holding a `default.project.json` named `place-<placeId>`, with
  `scope: "code"`, `servePlaceIds: [<placeId>]`, `gameId`, the usual services
  mapped to empty `src/<Service>` directories, and the runtime `ignoreGlobs`;
* with **`initialSync: "keep"`**, which is the whole point: a scratch project
  exists to *reach* a place, not to reconcile with it, so a connect moves
  nothing in either direction;
* served by a daemon started detached, exactly as
  `wsync daemon start --project <dir>` would.

### The scratch project cleans itself up

A scratch project is temporary by definition, so nothing is left for anyone
to delete. Its daemon is started `--ephemeral`, which is recorded in its
runtime record and means:

* once a Studio plugin has connected and then stayed away for 20 s, the
  daemon shuts itself down;
* a Studio that never connects at all ends the daemon after 5 minutes, so an
  abandoned launch does not leave one running;
* on the way out, the daemon process deletes its own project directory.

The delete is the only destructive thing place routing does, so its guard is
deliberately narrow: the directory must be named after a place id (digits
only) and sit directly inside one of the two scratch roots. A `--into`
directory never matches and is never deleted — and neither is anything else
on disk. `wsync daemon restart` preserves the flag, so a restarted scratch
daemon stays scratch.

`--keep` opts out of the auto-delete: the project and its daemon outlive
Studio, and `wsync places` shows the project as `attach` rather than
`attach (ephemeral)`.
