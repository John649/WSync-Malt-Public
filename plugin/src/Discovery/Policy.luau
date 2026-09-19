--!strict
-- Pure connection-flow policy (Design §9.2/§9.4 as amended; user-ruled
-- Ro-Sync model): portless auto-discovery is the only primary flow, and the
-- Not Connected page is a status card driven by a small state machine.
--
-- This module owns every decision that does not need a socket or a Roblox
-- API:
--
--   * candidate ordering  — the port this window was handed (a branch
--                           clone's marker, a place marker's `DaemonPort`,
--                           an attach the desktop app just made), then the
--                           Settings `Host:Port` override, then
--                           `127.0.0.1:7978-7990` (deduplicated); with
--                           `AutoDiscover = false` the Settings candidate is
--                           the whole list (pure manual mode)
--   * matching            — strict-GameId ranking of `/hello` responses
--                           against the open place
--   * the state machine   — idle → scanning → found/create-ready/no-broker,
--                           the two-step Create Project confirm (Confirm is
--                           the only transition that emits a create request),
--                           bounded daemon wait, retry/cancel with a
--                           generation counter that aborts stale async work
--   * presentation        — the status line, dot color and action button for
--                           every state (Design §9.2 vocabulary)
--
-- Discovery orchestrates the HTTP side; App executes the effects this
-- machine returns. No requires, no Roblox APIs — loadable under a standalone
-- `luau` binary for behavior tests (the Broker/WriteRules pattern).

local Policy = {}

Policy.LOCAL_HOST = "127.0.0.1"
Policy.PORT_MIN = 7978
Policy.PORT_MAX = 7990

-- Match ranks, higher wins; the scan may stop early at MATCH_EXACT.
Policy.MATCH_NONE = 0
Policy.MATCH_WEAK = 1 -- claimless daemon for an unpublished place, or a placeIds claim that excludes this place
Policy.MATCH_GAME = 2 -- gameId matches, no placeIds claim
Policy.MATCH_EXACT = 3 -- gameId and placeId both match

export type Candidate = {
	host: string,
	port: number,
	https: boolean,
}

export type Settings = {
	host: string,
	port: number,
	https: boolean?,
	autoDiscover: boolean,
	--- A branch clone's own daemon port, taken from the clone's marker. When
	--- present it is probed FIRST: the branch daemon is the only one that can
	--- match a clone, so walking the shared scan range to reach it is a dozen
	--- pointless connection refusals on every boot. Ranking is unchanged — it
	--- still has to come back MATCH_EXACT.
	branchPort: number?,
	--- The port the desktop app's attach route handed THIS window for THIS
	--- session (docs/places.md). Probed first and trusted exactly, because
	--- the daemon on the other end was started for this Studio and claims no
	--- ids to be recognised by.
	sessionPort: number?,
	--- The port the place's own `WSyncPlace` marker names, when it names one.
	--- Same shortcut as `branchPort`, same rule: still has to rank exact.
	markerPort: number?,
}

export type PlaceIdentity = {
	gameId: number,
	placeId: number,
	--- This Studio is a branch clone (docs/branches.md). Its `placeId` is the
	--- synthetic one derived from the branch name, so only the branch's own
	--- daemon can claim it.
	branch: boolean?,
	--- The branch name, when `branch` is true: a daemon serving this branch
	--- says so in its hello, which matches even before ids line up.
	branchName: string?,
	--- The `placeKey` this place carries on its `ServerStorage/WSyncPlace`
	--- marker (docs/places.md — "Unpublished places"). A daemon whose project
	--- names the same key ranks EXACT, which is the only way a place with no
	--- ids is told apart from every other place with no ids.
	placeKey: string?,
	--- The port the desktop app's attach route handed this window this
	--- session. A daemon answering there is this window's own, whatever it
	--- claims — see `rankHello`.
	sessionPort: number?,
}

export type HelloClaim = {
	gameId: number?,
	placeIds: { number }?,
	--- A branch daemon names the branch it serves (docs/branches.md).
	branch: string?,
	--- The `placeKey` this daemon's project claims (docs/places.md).
	placeKey: string?,
	--- A scratch daemon that stops itself once Studio goes away — an attach.
	ephemeral: boolean?,
	[string]: any,
}

--- Hosts the local discovery range already covers — used to deduplicate the
--- Settings candidate against the `127.0.0.1` port scan.
function Policy.isLoopback(host: string): boolean
	local hostname = string.lower(host)

	return hostname == "localhost" or hostname == "127.0.0.1" or hostname == "0.0.0.0"
end

--- The ordered `/hello` probe list: the Settings `Host:Port` override first
--- (it participates in discovery, so pointing it at a remote daemon needs no
--- extra UI), then the local range — minus the Settings address when it
--- already falls inside it. `autoDiscover = false` restricts the scan to the
--- Settings candidate alone.
function Policy.candidates(settings: Settings): { Candidate }
	local list: { Candidate } = {}
	local seen: { [number]: boolean } = {}

	--- The hinted loopback ports, in the order they are trusted: the one this
	--- session was handed, the branch clone's own, the place marker's.
	local function hint(port: number?)
		if port ~= nil and not seen[port] then
			seen[port] = true

			table.insert(list, {
				host = Policy.LOCAL_HOST,
				port = port,
				https = false,
			})
		end
	end

	hint(settings.sessionPort)
	hint(settings.branchPort)
	hint(settings.markerPort)

	local settingsIsHinted = Policy.isLoopback(settings.host) and seen[settings.port] == true

	if not settingsIsHinted then
		table.insert(list, {
			host = settings.host,
			port = settings.port,
			https = settings.https == true,
		})
	end

	if not settings.autoDiscover then
		return list
	end

	local coveredPort = if Policy.isLoopback(settings.host) then settings.port else nil

	for port = Policy.PORT_MIN, Policy.PORT_MAX do
		if port ~= coveredPort and not seen[port] then
			table.insert(list, {
				host = Policy.LOCAL_HOST,
				port = port,
				https = false,
			})
		end
	end

	return list
end

--- Rank a daemon's `/hello` claim against the open place (strict GameId):
--- for a published place only daemons claiming our GameId qualify — a
--- claimless daemon never matches — with `placeIds` refining among them; an
--- unpublished place takes the first claimless daemon.
---
--- A branch clone is stricter still: nothing below MATCH_EXACT counts
--- (docs/branches.md — "a clone that finds no daemon claiming its id refuses
--- to connect rather than falling through to main"), and it has one extra way
--- to reach exact — a daemon that names the branch it serves.
---
--- The branch-name rule is not a convenience. A clone opened from a local
--- `.rbxl` has `GameId == 0`, so there is no universe id for either side to
--- agree on; ids alone would leave the clone weak against every daemon and
--- the refusal below would then refuse them all.
--- `candidate` is where this hello came from, when the caller knows: it is
--- what makes the session-port hint meaningful, since the hint is about an
--- address rather than about anything the daemon says.
function Policy.rankHello(hello: HelloClaim, place: PlaceIdentity, candidate: Candidate?): number
	if place.branch ~= true then
		-- The mirror rule: a window without a marker never binds to a daemon
		-- that serves a branch, not even when main's own daemon is down —
		-- that would sync the live place against a branch worktree
		if hello.branch ~= nil and hello.branch ~= "" then
			return Policy.MATCH_NONE
		end

		-- The session hint: the desktop app started this daemon for THIS
		-- Studio window and told us the port. It claims no ids — it was made
		-- for a place that has none — so nothing below would ever rank it;
		-- what makes it exact is that we asked for it. Session state, never
		-- written into the place, so it dies with the window.
		if
			candidate ~= nil
			and place.sessionPort ~= nil
			and candidate.port == place.sessionPort
			and Policy.isLoopback(candidate.host)
		then
			return Policy.MATCH_EXACT
		end

		-- The marker rule: a place that carries a `placeKey` is reached by it.
		-- This is the id-free half of routing (docs/places.md), so it sits
		-- above the id rules rather than beside them — a place with no ids has
		-- nothing else to be told apart by.
		if place.placeKey ~= nil and place.placeKey ~= "" and hello.placeKey == place.placeKey then
			return Policy.MATCH_EXACT
		end

		-- A temporary daemon belongs to the one window it was made for. It is
		-- reached only by the session hint or the marker key above; any other
		-- window that scans past it (a second unpublished place, say) must
		-- attach its own rather than knock on a bridge that is already owned.
		if hello.ephemeral == true then
			return Policy.MATCH_NONE
		end

		return Policy.rankStrict(hello, place)
	end

	-- A clone answers ONLY to the daemon that names its branch. It now boots
	-- with main's real place id, so main's own daemon would rank exact by ids
	-- alone — and a clone whose daemon is down must refuse, never fall
	-- through to main and journal a branch's edits into the live place.
	if place.branchName ~= nil and place.branchName ~= "" and hello.branch == place.branchName then
		return Policy.MATCH_EXACT
	end

	return Policy.MATCH_NONE
end

--- Does this hello explicitly claim `placeId`?
function Policy.claimsPlace(hello: HelloClaim, placeId: number): boolean
	local placeIds = hello.placeIds

	return placeIds ~= nil and #placeIds > 0 and table.find(placeIds, placeId) ~= nil
end

--- The strict-GameId ranking itself, branch rules excluded.
function Policy.rankStrict(hello: HelloClaim, place: PlaceIdentity): number
	if place.gameId ~= 0 then
		-- Published place: unchanged. GameId gates everything, `placeIds`
		-- refines among the daemons that pass the gate.
		if hello.gameId ~= place.gameId then
			return Policy.MATCH_NONE
		end

		local placeIds = hello.placeIds

		if placeIds and #placeIds > 0 then
			return if table.find(placeIds, place.placeId) then Policy.MATCH_EXACT else Policy.MATCH_WEAK
		end

		return Policy.MATCH_GAME
	end

	-- No universe id to match on (an unpublished place, or a branch clone
	-- opened from a local file). A daemon that claims this exact placeId has
	-- made the stronger statement, so take it whatever it says about gameId.
	-- Guarded on a non-zero placeId: `placeIds: [0]` must not exactly match
	-- every unpublished place in the world.
	if place.placeId ~= 0 and Policy.claimsPlace(hello, place.placeId) then
		return Policy.MATCH_EXACT
	end

	if not hello.gameId or hello.gameId == 0 then
		return Policy.MATCH_WEAK
	end

	return Policy.MATCH_NONE
end

--- The index of the best-ranked hello in a batch of probe results, and its
--- rank. Ties go to the earlier candidate, which is what keeps the candidate
--- ordering meaningful once the probes run concurrently: among equally weak
--- matches the Settings address still wins, and a branch's own daemon port
--- still wins over everything, because both are first in the list.
---
--- `hellos` is sparse — a candidate that did not answer has no entry — so the
--- walk is driven by `count` rather than by `#hellos`. `candidates` is the
--- list those indices refer to, when the caller has it: `rankHello` needs it
--- for the session-port hint.
function Policy.bestIndex(
	hellos: { [number]: HelloClaim? },
	count: number,
	place: PlaceIdentity,
	candidates: { Candidate }?
): (number?, number)
	local bestIndex: number? = nil
	local bestRank = Policy.MATCH_NONE

	for index = 1, count do
		local hello = hellos[index]

		if hello ~= nil then
			local rank = Policy.rankHello(hello, place, if candidates then candidates[index] else nil)

			if rank > bestRank then
				bestIndex = index
				bestRank = rank
			end
		end
	end

	return bestIndex, bestRank
end

--------------------------------------------------------------------------------
-- Connection state machine
--------------------------------------------------------------------------------

export type StateName =
	"idle"
	| "scanning"
	| "found"
	| "attaching"
	| "create-ready"
	| "create-confirm"
	| "creating"
	| "waiting-daemon"
	| "no-broker"
	| "error"

export type State = {
	name: StateName,
	--- Bumped by every user-initiated transition; async completions carry the
	--- generation they started under and are ignored when it went stale
	generation: number,
	-- found: the daemon to connect to
	host: string?,
	port: number?,
	project: string?,
	-- create flow: the broker match the request goes to (opaque to the machine)
	broker: any?,
	-- create flow: the broker already has a project for this game (its hello
	-- `games` list names the open place's GameId), so the POST adopts and
	-- serves it instead of creating anything — no confirm ceremony needed
	known: boolean?,
	-- found: this connection is a temporary attach (docs/places.md) — a
	-- scratch daemon that disappears with this Studio window
	temporary: boolean?,
	-- waiting-daemon: bounded re-scan progress
	attempt: number?,
	total: number?,
	-- error: what went wrong
	message: string?,
}

export type EventKind =
	-- User intents
	"start" -- idle: the action button or AutoConnect
	| "cancel" -- scanning/found → idle; waiting-daemon → create-ready
	| "retry" -- no-broker/error: back to a fresh scan
	| "create" -- create-ready: confirm step for a new project; straight to the POST for a known one
	| "confirm" -- create-confirm: POST the request (step 2 — the only NEW-project emitter)
	| "back" -- create-confirm: return to create-ready (Escape does the same)
	| "reset" -- any: App dropped the flow (disconnect)
	-- Async completions (require a matching generation)
	| "scan-found"
	| "scan-none-broker"
	| "scan-none-attach" -- a broker that may hand this window a scratch daemon
	| "scan-none-no-broker"
	| "attach-ok"
	| "attach-none"
	| "create-ok"
	| "create-error"
	| "wait-progress"
	| "wait-found"
	| "wait-exhausted"

export type Event = {
	kind: EventKind,
	generation: number?,
	host: string?,
	port: number?,
	project: string?,
	broker: any?,
	known: boolean?,
	temporary: boolean?,
	attempt: number?,
	total: number?,
	message: string?,
}

--- What App must do after a transition: run the candidate scan, hand the
--- resolved address to Core, POST `/projects/init`, or start the bounded
--- daemon wait.
export type Effect = "scan" | "connect" | "create" | "wait" | "attach"

local WAIT_EXHAUSTED_MESSAGE = "The daemon did not appear — check the WSync app, then retry"
local CREATE_FAILED_MESSAGE = "the WSync app could not create the project"

--- The line the connected card shows while the connection is a temporary
--- attach: a scratch daemon that disappears with this Studio window
--- (docs/places.md). Nil for an ordinary project connection, which is what
--- the card shows nothing extra for.
function Policy.temporaryNote(hello: HelloClaim?): string?
	if hello == nil or hello.ephemeral ~= true then
		return nil
	end

	return "Temporary attach — nothing is written to disk; it closes with this window"
end

function Policy.initialState(): State
	return {
		name = "idle",
		generation = 0,
	}
end

local function make(name: StateName, generation: number, extra: { [string]: any }?): State
	local state: { [string]: any } = {
		name = name,
		generation = generation,
	}

	if extra then
		for key, value in extra do
			state[key] = value
		end
	end

	return (state :: any) :: State
end

--- Advance the machine. Returns the next state plus the effect App must run.
--- An event that does not apply — wrong source state, or an async completion
--- whose generation went stale — returns the *same* state table (identity)
--- and no effect, so callers can cheaply skip no-op updates.
function Policy.step(state: State, event: Event): (State, Effect?)
	local kind = event.kind

	-- User intents ----------------------------------------------------------

	if kind == "start" then
		if state.name == "idle" then
			return make("scanning", state.generation + 1), "scan"
		end
	elseif kind == "cancel" then
		if state.name == "scanning" or state.name == "found" or state.name == "attaching" then
			return make("idle", state.generation + 1), nil
		elseif state.name == "waiting-daemon" then
			return make("create-ready", state.generation + 1, { broker = state.broker, known = state.known }), nil
		end
	elseif kind == "retry" then
		if state.name == "no-broker" or state.name == "error" then
			return make("scanning", state.generation + 1), "scan"
		end
	elseif kind == "force-create" then
		-- The card's "Make permanent". Unconditional on purpose: the window
		-- was attached, so dropping the transport leaves the machine free to
		-- rescan, and the place now carries the scratch daemon's key — a
		-- state-guarded create would lose the race against that reconnect
		return make("creating", state.generation + 1, {
			broker = event.broker,
			known = event.known == true,
		}),
			"create"
	elseif kind == "create" then
		if state.name == "create-ready" then
			-- A known game skips the confirm ceremony: the POST is an
			-- idempotent adopt-and-serve of the project the broker already
			-- has, not a creation (the broker's existing-gameId rule)
			if state.known then
				return make("creating", state.generation + 1, { broker = state.broker, known = true }), "create"
			end

			return make("create-confirm", state.generation, { broker = state.broker }), nil
		end
	elseif kind == "confirm" then
		if state.name == "create-confirm" then
			-- The one and only transition that emits a NEW-project request
			return make("creating", state.generation + 1, { broker = state.broker }), "create"
		end
	elseif kind == "back" then
		if state.name == "create-confirm" then
			return make("create-ready", state.generation, { broker = state.broker }), nil
		end
	elseif kind == "reset" then
		return make("idle", state.generation + 1), nil
	else
		-- Async completions -------------------------------------------------

		if event.generation ~= state.generation then
			return state, nil
		end

		if kind == "scan-found" then
			if state.name == "scanning" then
				return make("found", state.generation, {
					host = event.host,
					port = event.port,
					project = event.project,
				}),
					"connect"
			end
		elseif kind == "scan-none-broker" then
			if state.name == "scanning" then
				return make("create-ready", state.generation, {
					broker = event.broker,
					known = event.known == true,
				}),
					nil
			end
		elseif kind == "scan-none-attach" then
			-- No daemon, but a broker that can hand this window a throwaway
			-- one. No click: the whole point of Attach is that opening a place
			-- is enough (docs/places.md). The fall-back is the ordinary
			-- create-ready card, which `attach-none` returns to, so a refusal
			-- or a switched-off setting costs nothing but a round trip.
			if state.name == "scanning" then
				return make("attaching", state.generation, {
					broker = event.broker,
					known = event.known == true,
				}),
					"attach"
			end
		elseif kind == "attach-ok" then
			if state.name == "attaching" then
				return make("found", state.generation, {
					host = event.host,
					port = event.port,
					project = event.project,
					temporary = true,
				}),
					"connect"
			end
		elseif kind == "attach-none" then
			-- The broker said no (switched off, or it could not). Show what
			-- the card would have shown without the attach at all.
			if state.name == "attaching" then
				return make("create-ready", state.generation, {
					broker = state.broker,
					known = state.known,
				}),
					nil
			end
		elseif kind == "scan-none-no-broker" then
			if state.name == "scanning" then
				return make("no-broker", state.generation), nil
			end
		elseif kind == "create-ok" then
			if state.name == "creating" then
				return make("waiting-daemon", state.generation, { broker = state.broker, known = state.known }), "wait"
			end
		elseif kind == "create-error" then
			if state.name == "creating" then
				return make("error", state.generation, {
					message = event.message or CREATE_FAILED_MESSAGE,
				}),
					nil
			end
		elseif kind == "wait-progress" then
			if state.name == "waiting-daemon" then
				return make("waiting-daemon", state.generation, {
					broker = state.broker,
					attempt = event.attempt,
					total = event.total,
				}),
					nil
			end
		elseif kind == "wait-found" then
			if state.name == "waiting-daemon" then
				return make("found", state.generation, {
					host = event.host,
					port = event.port,
					project = event.project,
				}),
					"connect"
			end
		elseif kind == "wait-exhausted" then
			if state.name == "waiting-daemon" then
				return make("error", state.generation, { message = WAIT_EXHAUSTED_MESSAGE }), nil
			end
		end
	end

	return state, nil
end

--------------------------------------------------------------------------------
-- Presentation
--------------------------------------------------------------------------------

export type Dot = "neutral" | "green" | "amber" | "red"

export type Env = {
	placeName: string,
	autoDiscover: boolean,
	host: string,
	port: number,
}

export type Descriptor = {
	status: string,
	dot: Dot,
	--- Label of the single action button; nil while a broker request is in
	--- flight (nothing to click)
	action: string?,
	--- Whether the confirm step's secondary Back button is shown
	back: boolean,
}

--- The status line, dot color and button label for a state (Design §9.2
--- vocabulary). `env` carries the place name and the Settings values so the
--- idle/scanning lines stay truthful in pure manual mode.
function Policy.describe(state: State, env: Env): Descriptor
	local name = state.name

	if name == "idle" then
		return {
			status = if env.autoDiscover
				then `Auto-discover {Policy.LOCAL_HOST}:{Policy.PORT_MIN}–{Policy.PORT_MAX}`
				else `Connect to {env.host}:{env.port}`,
			dot = "neutral" :: Dot,
			action = "Connect",
			back = false,
		}
	elseif name == "scanning" then
		return {
			status = if env.autoDiscover
				then `Scanning localhost {Policy.PORT_MIN}–{Policy.PORT_MAX}…`
				else `Probing {env.host}:{env.port}…`,
			dot = "neutral" :: Dot,
			action = "Cancel",
			back = false,
		}
	elseif name == "attaching" then
		local placeName = if env.placeName ~= "" then env.placeName else "this place"

		return {
			status = `Attaching to '{placeName}' — temporary, nothing is written to disk…`,
			dot = "amber" :: Dot,
			action = "Cancel",
			back = false,
		}
	elseif name == "found" then
		local project = state.project or "project"
		local where = if state.host == nil or Policy.isLoopback(state.host :: string)
			then `port {state.port}`
			else `{state.host}:{state.port}`

		return {
			status = if state.temporary
				then `Attached temporarily on {where} — connecting…`
				else `Found '{project}' on {where} — connecting…`,
			dot = "green" :: Dot,
			action = "Cancel",
			back = false,
		}
	elseif name == "create-ready" then
		-- A known game is the app's own project with its daemon down: the
		-- button serves it, and saying "Create" here would read as a duplicate
		if state.known then
			local placeName = if env.placeName ~= "" then env.placeName else "This place"

			return {
				status = `'{placeName}' is already in your WSync Projects · daemon not running`,
				dot = "amber" :: Dot,
				action = "Serve Project",
				back = false,
			}
		end

		return {
			status = "New Studio project · ready to create",
			dot = "amber" :: Dot,
			action = "Create Project",
			back = false,
		}
	elseif name == "create-confirm" then
		local placeName = if env.placeName ~= "" then env.placeName else "this place"

		return {
			status = `Create '{placeName}' in your WSync Projects folder?`,
			dot = "amber" :: Dot,
			action = "Confirm",
			back = true,
		}
	elseif name == "creating" then
		return {
			status = if state.known then "Starting the project daemon…" else "Creating project…",
			dot = "amber" :: Dot,
			action = nil,
			back = false,
		}
	elseif name == "waiting-daemon" then
		return {
			status = if state.attempt
				then `Project ready · waiting for the daemon… ({state.attempt}/{state.total})`
				else "Project ready · waiting for the daemon…",
			dot = "amber" :: Dot,
			action = "Cancel",
			back = false,
		}
	elseif name == "no-broker" then
		return {
			status = "No WSync daemon for this place. Open the WSync desktop app "
				.. "and choose a Projects folder (Settings) to enable project creation.",
			dot = "red" :: Dot,
			action = "Retry",
			back = false,
		}
	end

	return {
		status = state.message or "Connection error",
		dot = "red" :: Dot,
		action = "Retry",
		back = false,
	}
end

return Policy
