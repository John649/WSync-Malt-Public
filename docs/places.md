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

A place that was never published has no id to be named by. It gets one:
a **place key**, carried on a marker in the place itself — see
[Unpublished places](#unpublished-places).

## `wsync places`

Lists every runtime record under the state directory (`%LOCALAPPDATA%\WSync\daemons\*.json`
on Windows) and probes `GET /hello` on the port each one names. It starts,
stops and writes nothing.

```
PROJECT                        PORT  GAME     PLACES      KEY                 BRANCH   MANAGED BY  PLUGIN      KIND
DungeonDemo                    7970  5550001  128374926   -                   main     cli         WSync (ws)  project
DungeonDemo                    7971  5550001  128374926   -                   ui-pass  cli         waiting     project
place-88120033                 7973  4410902  88120033    -                   main     cli         WSync (ws)  attach (ephemeral)
unpublished-my-level-ab12cd34  7974  -        -           my-level-ab12cd34   main     desktop     WSync (ws)  attach (ephemeral)
```

* **PLACES** comes from the daemon's own `/hello` when it answers, and from
  its project file's `servePlaceIds` when it does not. A record whose daemon
  is gone is still listed, with `PLUGIN` reading `unreachable` — a stale
  record should be visible, not silent.
* **PLUGIN** is `waiting` when the daemon is up but no Studio plugin has the
  bridge, and `<name> (ws|long-poll)` when one does.
* **KEY** is the project's `placeKey`, when it sets one — the name an
  unpublished place is routed by. `-` for every project addressed by ids.
* **KIND** marks the scratch projects `wsync open` and the desktop app's
  attach route create: `attach`, or `attach (ephemeral)` when the project
  deletes itself once Studio closes.

`--raw` prints the same rows as one JSON array.

## `--place <placeId | placeKey>`

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

A single all-digit argument is a place id; anything else is a **place key**,
and step 1 keeps the daemons whose project names that exact key instead. The
rest of the rules are identical:

```sh
wsync get --place my-level-ab12cd34 ReplicatedStorage/Shared/Config
```

The two forms cannot collide — an id is digits, a key never is — and nothing
launches Studio for a key: the place it names exists only as a file somebody
already has open, so an unclaimed key says so rather than offering
`wsync open`.

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
deliberately narrow: the directory must carry a **scratch name** — a place id
(digits only), or `unpublished-<slug>-<hex>` for a place that has no id — and
sit directly inside one of the two scratch roots. A `--into` directory never
matches and is never deleted — and neither is anything else on disk.
`wsync daemon restart` preserves the flag, so a restarted scratch daemon
stays scratch.

`--keep` opts out of the auto-delete: the project and its daemon outlive
Studio, and `wsync places` shows the project as `attach` rather than
`attach (ephemeral)`.

## Attach from Studio

`wsync open <placeId>` is one half of attach: you know the id, and you want
Studio opened on it. The other half is the case with no id and no command —
somebody already has a place open, published or not, and wants it connected.

When the plugin's discovery scan finds no daemon for the open place and the
desktop app's broker is reachable, it attaches **automatically**. There is no
button: opening the place is the whole gesture. The card shows the connection
with one extra line —

> Temporary attach — nothing is written to disk; it closes with this window

— and the ordinary "Create Project" affordance stays exactly where it was, so
turning a temporary connection into a permanent project is still one click.

### The route

```
POST http://127.0.0.1:<brokerPort>/places/attach
{ "placeId": 0, "gameId": 0, "name": "My Level", "sessionId": "<hex>" }

200 { "ok": true, "port": 7979, "project": "<dir>/default.project.json",
      "name": "unpublished-my-level-ab12cd34",
      "key": "unpublished-my-level-ab12cd34",
      "placeKey": "my-level-ab12cd34", "ephemeral": true }
```

* `placeId`/`gameId` are `0` for an unpublished place, which is the case the
  route exists for. `sessionId` is the plugin's per-window identity, 8–128 hex
  characters.
* The broker advertises the route in its own `/hello` (`attach: true`), so an
  older desktop app costs no round trip.
* It answers `200 {"ok": false, "disabled": true}` when the feature is
  switched off, which is not an error: the plugin falls back to the ordinary
  no-daemon card.
* A path-shaped field in the request is **refused**, exactly as it is on
  `/projects/init`: WSync derives the folder and never takes one from Studio.

### What it creates

One directory under the authorized projects folder:

```
<projectsRoot>/_places/<key>/
```

where `<key>` is the **place id** for a published place, and
`unpublished-<slug(name)>-<8 hex of sessionId>` for one without. The session
half is what keeps two windows on two copies of the same unnamed baseplate
from claiming one folder — and what makes the same window asking twice land
on the project it already has.

The project file is written by the engine itself
(`wsync open --attach-only --into <dir>`), so a scratch project has exactly
one implementation: code scope, `initialSync: "keep"`, the mapped service
directories, and — for an unpublished place — a `placeKey` and no ids at all.

Its daemon is started through the desktop app's ordinary daemon lifecycle
(owner token, supervisor, heartbeat) plus `--ephemeral`, so it cleans itself
up exactly as the `wsync open` scratch daemons above do. It is held under the
project id `attach:<key>`, which nothing writes to `state.json` and which the
Projects view ignores: an attach is not a project.

### Why the port is trusted

A scratch daemon for an unpublished place claims no ids — there are none — so
no ranking rule could recognise it. The plugin therefore trusts the port the
broker just handed *this* window, for *this* session, as an exact match. That
hint is session state in the plugin: it is never written into the place, it
cannot outlive the window, and it cannot reach another one.

It also survives a disconnect, so a reconnect finds the same daemon still
serving this window. When that daemon is gone the scan finds nothing and the
card returns to the ordinary discovery states — one automatic attach per place
per Studio session, so nothing can loop. **Retry** is what asks for another.

Branch clones are never attached: a clone answers only to the daemon that
names its branch (docs/branches.md), and those rules are untouched.

## Unpublished places

A published place is reached by its ids. An unpublished one has neither
`placeId` nor `gameId` — both read `0` — so no daemon can say anything about
it that tells it apart from every other unpublished place. Two pieces give it
a name of its own:

| | |
|---|---|
| `placeKey` in the project file | what the project claims, reported by `/hello` |
| `ServerStorage/WSyncPlace` | a `StringValue` in the place whose `Value` is that key |

A Studio whose place carries the marker ranks a daemon **EXACT** when the
daemon's `placeKey` equals the marker's value. That rule sits above the
id-based ones — a place with no ids has nothing else to be told apart by — and
below the branch rules, which are unchanged: a clone still answers only to its
branch's daemon, and a place key on one is never even read.

An unmarked unpublished place keeps today's behaviour: the first claimless
daemon, weakly.

The marker may also carry a number attribute `DaemonPort`, which discovery
probes first. A shortcut, not an override — the port still has to rank exact.

### Who writes it

The plugin, and only the plugin. The engine never touches Studio and the
desktop app never touches a place. After a connect to a project that names a
`placeKey`, if the place carries no marker yet, the plugin creates the
`StringValue` with the documented `AvoidSync` attribute — so it never syncs in
either direction, in either scope — and the key lands in the `.rbxl` on the
next save. A marker that already names a *different* key is never overwritten.

### Where the key comes from

The desktop app mints it for an automatic attach: `slug(name)` plus eight hex
characters of that Studio session. It is stable for as long as the window
lives, which is as long as the scratch project it names exists.

When the same place is later given a **permanent** project through the create
flow, the plugin sends the key the place already carries and the broker writes
it into the new project file. From then on the place reconnects to that
project by name, with no attach involved.

Published places keep id-based routing. A `placeKey` on one is allowed and
simply unnecessary.
