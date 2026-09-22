# The hub

Every daemon on a machine is reached through one address:

```
http://127.0.0.1:7960/p/<route>/...
```

The **hub** listens on `127.0.0.1:7960`. Each daemon binds whatever port the
OS hands it, registers with the hub, and is reached through it at its
**route**. The Studio plugin, the CLI and the desktop app all talk to the hub.
None of them ever needs to know a daemon's port.

## Why

Daemons used to take a port from `7978–7990`, and everything found them by
that number. The plugin scanned the range, the CLI trusted the port in a
runtime record, and a branch stored it in `branch.json` and in the clone's
marker. A port number is not an identity, and it failed that way:

- sixteen daemons overflowed a thirteen-port range, and the plugin never
  scanned the overflow;
- a daemon that died left a record naming a port another project then took;
- `daemon restart` insisted on the old port and failed when it was taken;
- a branch daemon that came back on a different port was lost to its clone
  until someone hand-edited `branch.json`.

A route names a **project**, not a process. It is the first 16 hex digits of
the SHA-256 of the canonical project path, so it is the same before and after
a restart, on whatever port the daemon ends up. A daemon that is gone is
simply absent from the hub, and nothing else can answer in its place.

## Registration

A daemon opens `ws://127.0.0.1:7960/hub/attach` and sends one frame:

```json
{ "kind": "daemon", "canonicalProject": "…/default.project.json",
  "port": 51770, "bootId": "…", "pid": 4242 }
```

Before it answers, the hub checks the claim against the daemon's own
`GET /hello` on that port: the project and boot must match. Then it replies
`{"type": "registered", "route": "…"}`, and the registration lasts exactly as
long as that socket stays open.

- **A daemon that exits** closes its socket, so its route is gone at once.
  A daemon that hangs stops answering the hub's pings and is dropped after
  20 s.
- **One project, one daemon.** A second registration for the same project is
  refused while the first still answers as itself, with a reason naming the
  PID that holds it.
- **The desktop app's broker** registers the same way as
  `{"kind": "broker", "port": …, "pid": …}` and is reached at `/broker`.

## Keeping a hub running

There is no service to install. `wsync daemon start` (and `serve`) starts a
hub when none answers. So does every registered daemon that loses its hub,
and so does the desktop app. Binding the port is the only lock: a second hub
finds `7960` taken and exits. Daemons register again within a quarter of a
second, and a Studio connected through a hub that died reconnects on its own
(measured on a Mac: about a second).

A hub with nothing registered exits after a minute.

The hub process is `wsync daemon hub`, and it logs to `hub.log` in the state
directory. `WSYNC_HUB_PORT` moves it. Tests set it so they never touch the
real one.

## Routes on the hub

| Route | What it is |
| --- | --- |
| `GET /hello` | `{"hub": true, "hubProtocol": 1, "version", "pid", "port", "daemons", "broker"}`. It has no numeric `protocol`, so an older plugin never mistakes the hub for a daemon |
| `GET /daemons` | every registered daemon (`route`, `base`, `canonicalProject`, `port`, `pid`, `bootId`, `hello`) in registration order, plus `broker` |
| `GET /hub/attach` | the registration WebSocket |
| `/p/<route>/…` | forwarded to that daemon, WebSocket upgrades included; `404` when nothing is registered there |
| `/broker/…` | forwarded to the desktop app's broker |

The hub refuses a `Host` header that is not its own loopback address (DNS
rebinding) and any `Origin` a web page would send. The desktop app's webview
origins are allowed.

## Who uses what

- **Studio plugin.** Discovery is one `GET /daemons`, ranked by the same rules
  as before (docs/places.md, docs/branches.md). The plugin then connects to
  `/p/<route>`. When a daemon restarts, the same address reaches its
  replacement, and the plugin resyncs on its own.
- **CLI.** `--project` goes through the project's route. `--port` still talks
  to a daemon's own port directly, as an escape hatch and for a stock Argon
  plugin. `wsync places` and `--place` read `GET /daemons`, and
  `wsync daemon status` reports `routed` and `route`.
- **Desktop app.** It registers its broker with the hub. It still talks to
  the daemons it spawned on the port their ready line reports, because that
  port comes from the process itself rather than from a file.

## Pinned ports

A daemon still binds a fixed port when told to, with `daemon start --port N`
or the project file's `port`. That pinned port is exact: if it is taken, the
start fails rather than moving somewhere else. A pinned daemon registers with
the hub like any other. Pinning exists for clients that cannot reach the hub,
such as a stock Argon plugin.

## Upgrading

A daemon started by a build from before the hub does not register with it,
so nothing built with the hub can reach it. `wsync places` lists it as
`unreachable`, and `wsync daemon start` on its project says it "predates the
hub". Restart it: `wsync daemon restart --project <path>`.

The Studio plugin must come from the same build (`wsync plugin install`).
