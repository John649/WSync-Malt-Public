-- Discovery's pure rules for place routing and the automatic attach
-- (docs/places.md). Runs the real `Discovery/Policy` and `Broker` modules —
-- both require-free — under standalone luau; `ref` and `require` arrive from
-- harness/loader.luau.
--
-- What is covered: the `placeKey` marker rank that makes an unpublished place
-- routable at all, the session-route hint an attach is trusted by, the branch
-- rules staying untouched by both, where a scan looks and how a hub's
-- listing becomes candidates, the attach states in the connection machine,
-- and the request/response shapes the attach route is spoken to with.

local Policy = require(ref("Policy"))
local Broker = require(ref("Broker"))

local failures = 0
local checks = 0

local function expect(condition: boolean, label: string)
	checks += 1

	if condition then
		print(`  ok    {label}`)
	else
		failures += 1
		print(`  FAIL  {label}`)
	end
end

local function place(fields: { [string]: any }): Policy.PlaceIdentity
	local identity: { [string]: any } = { gameId = 0, placeId = 0 }

	for key, value in fields do
		identity[key] = value
	end

	return (identity :: any) :: Policy.PlaceIdentity
end

local function at(route: string): Policy.Candidate
	return { host = "127.0.0.1", port = Policy.HUB_PORT, https = false, base = `/p/{route}`, route = route }
end

--------------------------------------------------------------------------------
print("the placeKey marker")
--------------------------------------------------------------------------------

do
	local marked = place({ placeKey = "my-level-ab12cd34" })

	expect(
		Policy.rankHello({ placeKey = "my-level-ab12cd34" }, marked) == Policy.MATCH_EXACT,
		"a daemon naming the marker's key matches exactly, with no ids on either side"
	)

	expect(
		Policy.rankHello({ placeKey = "somebody-elses-key" }, marked) == Policy.MATCH_NONE,
		"a daemon keyed to another place is refused, not taken as a claimless fallback"
	)

	expect(
		Policy.rankHello({ placeKey = "my-level-ab12cd34" }, place({})) == Policy.MATCH_NONE,
		"a place with no marker never adopts a keyed project — every unpublished place would match it"
	)

	expect(
		Policy.rankHello({}, place({})) == Policy.MATCH_WEAK,
		"an unkeyed claimless daemon still takes an unpublished place, which is how `wsync init` is reached"
	)

	expect(
		Policy.rankHello({ placeKey = "my-level-ab12cd34", gameId = 12345 }, marked) == Policy.MATCH_EXACT,
		"the key beats the ids: a daemon for another universe still matches the key it names"
	)

	-- A published place keeps id routing; a key on it is allowed and unused
	local published = place({ gameId = 42, placeId = 7, placeKey = "my-level-ab12cd34" })

	expect(
		Policy.rankHello({ gameId = 42, placeIds = { 7 } }, published) == Policy.MATCH_EXACT,
		"a published place still matches on its ids without any key in the hello"
	)

	expect(
		Policy.rankHello({ gameId = 999 }, published) == Policy.MATCH_NONE,
		"...and a wrong universe is still a refusal"
	)

	-- Unmarked and unpublished: exactly today's behaviour
	local bare = place({})

	expect(
		Policy.rankHello({}, bare) == Policy.MATCH_WEAK,
		"an unmarked unpublished place still takes the first claimless daemon, weakly"
	)

	-- This used to be a weak match, and that was the bug: a keyed project
	-- belongs to one unpublished place, and a place that carries no key is
	-- not that place. Seen live — a brand-new baseplate adopted another
	-- place's project and began writing into it.
	expect(
		Policy.rankHello({ placeKey = "anything" }, bare) == Policy.MATCH_NONE,
		"...and a keyed daemon is refused by a place that carries no key"
	)

	expect(
		Policy.rankHello({ placeKey = "" }, place({ placeKey = "" })) == Policy.MATCH_WEAK,
		"an empty key on both sides is not a match: empty is not an identity"
	)
end

--------------------------------------------------------------------------------
print("the session-route hint")
--------------------------------------------------------------------------------

do
	local attached = place({ sessionRoute = "0123456789abcdef" })

	expect(
		Policy.rankHello({}, attached, at("0123456789abcdef")) == Policy.MATCH_EXACT,
		"the route the app handed this window is exact, though the daemon claims nothing"
	)

	expect(
		Policy.rankHello({}, attached, at("fedcba9876543210")) == Policy.MATCH_WEAK,
		"...and only that route: every other claimless daemon ranks as it always did"
	)

	expect(
		Policy.rankHello({}, attached) == Policy.MATCH_WEAK,
		"a caller that does not say where the hello came from cannot take the hint"
	)

	expect(
		Policy.rankHello({}, attached, {
			host = "example.com",
			port = Policy.HUB_PORT,
			https = false,
			base = "/p/0123456789abcdef",
			route = "0123456789abcdef",
		}) == Policy.MATCH_WEAK,
		"the hint is a route on this machine's hub, not on any hub in the world"
	)

	expect(
		Policy.rankHello({}, attached, { host = "127.0.0.1", port = 7979, https = false, base = "" })
			== Policy.MATCH_WEAK,
		"a daemon reached on its own port has no route, so it can never be the attach"
	)

	expect(
		Policy.rankHello({ branch = "ui-pass" }, attached, at("0123456789abcdef")) == Policy.MATCH_NONE,
		"the mirror rule still wins: an unmarked window never binds to a branch daemon"
	)

	expect(
		Policy.rankHello({ gameId = 999 }, place({ gameId = 42, sessionRoute = "0123456789abcdef" }), at("0123456789abcdef"))
			== Policy.MATCH_EXACT,
		"a published place that asked for an attach trusts the route it was given"
	)
end

--------------------------------------------------------------------------------
print("branch clones are untouched")
--------------------------------------------------------------------------------

do
	local clone = place({
		branch = true,
		branchName = "ui-pass",
		placeKey = "my-level-ab12cd34",
		sessionRoute = "0123456789abcdef",
	})

	expect(
		Policy.rankHello({ branch = "ui-pass" }, clone, at("1111111111111111")) == Policy.MATCH_EXACT,
		"a clone still matches the daemon that names its branch"
	)

	expect(
		Policy.rankHello({ placeKey = "my-level-ab12cd34" }, clone, at("0123456789abcdef")) == Policy.MATCH_NONE,
		"neither a matching key nor the session route reaches a clone: only its branch does"
	)

	expect(
		Policy.rankHello({ gameId = 0, placeIds = {} }, clone, at("0123456789abcdef")) == Policy.MATCH_NONE,
		"...and a clone whose branch daemon is absent still refuses rather than falling through"
	)
end

--------------------------------------------------------------------------------
print("where a scan looks")
--------------------------------------------------------------------------------

do
	local default = Policy.sources({ host = "localhost", port = Policy.HUB_PORT, autoDiscover = true })

	expect(#default == 1, "the default Settings address is the hub, asked once")
	expect(default[1].host == "127.0.0.1", "...on the IPv4 loopback the hub binds, whatever Host says")

	local pinned = Policy.sources({ host = "127.0.0.1", port = 7985, autoDiscover = true })

	expect(#pinned == 2, "a Settings address that is not the hub is asked, and then the hub")
	expect(pinned[1].port == 7985 and pinned[2].port == Policy.HUB_PORT, "...in that order")

	local manual = Policy.sources({ host = "10.0.0.5", port = 9000, autoDiscover = false })

	expect(#manual == 1 and manual[1].host == "10.0.0.5", "manual mode is still the Settings address alone")

	expect(Policy.isHubHello({ hub = true, hubProtocol = 1 }), "a hub's hello is recognised")
	expect(not Policy.isHubHello({ protocol = 1 }), "...and a daemon's is not")
end

--------------------------------------------------------------------------------
print("a hub's listing")
--------------------------------------------------------------------------------

do
	local hub = { host = "127.0.0.1", port = Policy.HUB_PORT, https = false }

	local candidates, hellos = Policy.fromListing({
		daemons = {
			{ route = "aaaaaaaaaaaaaaaa", hello = { protocol = 1, gameId = 42, placeIds = { 7 } } },
			{ route = "bbbbbbbbbbbbbbbb", hello = nil },
			{ route = "../../etc", hello = { protocol = 1 } },
			{ route = "cccccccccccccccc", hello = { name = "no protocol" } },
			{ route = "dddddddddddddddd", hello = { protocol = 1, branch = "ui-pass" } },
		},
	}, hub)

	expect(#candidates == 2 and #hellos == 2, "only daemons that answered, with a usable route, become candidates")
	expect(candidates[1].base == "/p/aaaaaaaaaaaaaaaa", "a candidate is reached at its route on the hub")
	expect(candidates[1].port == Policy.HUB_PORT and candidates[1].route == "aaaaaaaaaaaaaaaa", "...through the hub")
	expect(hellos[2].branch == "ui-pass", "hellos line up with their candidates")

	local published = place({ gameId = 42, placeId = 7 })

	expect(
		Policy.bestIndex(hellos, #candidates, published, candidates) == 1,
		"main's daemon wins for main's window, and the branch daemon never does"
	)

	local clone = place({ gameId = 42, placeId = 7, branch = true, branchName = "ui-pass" })

	expect(Policy.bestIndex(hellos, #candidates, clone, candidates) == 2, "the clone takes its branch daemon")

	local empty = Policy.fromListing(nil, hub)

	expect(#empty == 0, "no listing is no candidates")
end

--------------------------------------------------------------------------------
print("the attach states")
--------------------------------------------------------------------------------

do
	local state = Policy.initialState()
	local effect

	state, effect = Policy.step(state, { kind = "start" })
	expect(state.name == "scanning" and effect == "scan", "start scans")

	local broker = { host = "127.0.0.1", port = Policy.HUB_PORT, base = "/broker" }
	local scanning = state

	state, effect = Policy.step(state, { kind = "scan-none-attach", generation = state.generation, broker = broker })
	expect(state.name == "attaching" and effect == "attach", "no daemon plus an attach-capable broker attaches")
	expect(state.broker == broker, "the broker rides along so the request knows where to go")

	local attaching = state

	state, effect = Policy.step(state, {
		kind = "attach-ok",
		generation = state.generation,
		host = "127.0.0.1",
		port = Policy.HUB_PORT,
		base = "/p/0123456789abcdef",
		project = "unpublished-my-level-ab12cd34",
	})
	expect(state.name == "found" and effect == "connect", "the answer connects straight to the route")
	expect(state.base == "/p/0123456789abcdef" and state.temporary == true, "...and the connection is marked temporary")

	-- A refusal is the ordinary no-daemon card, not an error page
	local refused, refusedEffect = Policy.step(attaching, { kind = "attach-none", generation = attaching.generation })
	expect(refused.name == "create-ready" and refusedEffect == nil, "a refusal falls back to the create offer")
	expect(refused.broker == broker, "...still holding the broker the offer needs")

	local canceled = Policy.step(attaching, { kind = "cancel" })
	expect(canceled.name == "idle", "cancel while attaching returns to idle")

	local stale = Policy.step(attaching, { kind = "attach-ok", generation = attaching.generation - 1, port = 1 })
	expect(stale == attaching, "an answer from a superseded attempt changes nothing")

	local offered = Policy.step(scanning, {
		kind = "scan-none-broker",
		generation = scanning.generation,
		broker = broker,
	})
	expect(offered.name == "create-ready", "a broker that cannot attach still offers to create, exactly as before")
end

--------------------------------------------------------------------------------
print("what the card says")
--------------------------------------------------------------------------------

do
	local env = { placeName = "My Level", autoDiscover = true, host = "127.0.0.1", port = 7985 }

	local attaching = Policy.describe({ name = "attaching", generation = 1 }, env)
	expect(string.find(attaching.status, "My Level") ~= nil, "the attaching line names the place")
	expect(string.find(attaching.status, "nothing is written to disk") ~= nil, "...and says what temporary means")
	expect(attaching.action == "Cancel", "an attach can be called off")

	local temporary =
		Policy.describe({ name = "found", generation = 1, port = 7960, base = "/p/0123456789abcdef", temporary = true }, env)
	expect(string.find(temporary.status, "Attached temporarily") ~= nil, "a temporary connect says so while connecting")

	local ordinary =
		Policy.describe({ name = "found", generation = 1, port = 7960, base = "/p/0123456789abcdef", project = "dungeon" }, env)
	expect(string.find(ordinary.status, "Found 'dungeon'") ~= nil, "an ordinary connect names the project")
	expect(string.find(ordinary.status, "port") == nil, "...and no port: a hub route has none worth showing")

	local direct = Policy.describe({ name = "found", generation = 1, port = 7985, base = "", project = "dungeon" }, env)
	expect(string.find(direct.status, "port 7985") ~= nil, "a daemon reached on its own port says which")

	expect(Policy.temporaryNote({ ephemeral = true }) ~= nil, "an ephemeral daemon earns the connected-card note")
	expect(Policy.temporaryNote({ ephemeral = false }) == nil, "an ordinary daemon does not")
	expect(Policy.temporaryNote(nil) == nil, "and neither does no daemon at all")
end

--------------------------------------------------------------------------------
print("the attach request and its answer")
--------------------------------------------------------------------------------

do
	local session = string.rep("ab", 16)

	local body = Broker.shapeAttachRequest({ gameId = 0, placeId = 0, placeName = "My Level" }, session)
	expect(body ~= nil, "an unpublished place shapes a request")
	expect(body.placeId == 0 and body.gameId == 0, "...carrying the zero ids it actually has")
	expect(body.name == "My Level", "...and what the place calls itself")
	expect(body.sessionId == session, "...and this window's session id")

	local published = Broker.shapeAttachRequest({ gameId = 42, placeId = 7 }, session)
	expect(published.gameId == 42 and published.placeId == 7, "a published place sends its ids")
	expect(published.name == nil, "...and no empty name field")

	expect(Broker.shapeAttachRequest({ gameId = 0, placeId = 0 }, "nothex!!") == nil, "a non-hex session is refused")
	expect(Broker.shapeAttachRequest({ gameId = 0, placeId = 0 }, "abc") == nil, "...and so is a too-short one")
	expect(Broker.shapeAttachRequest({ gameId = -1, placeId = 0 }, session) == nil, "...and a nonsense id")

	local ok, failure, disabled = Broker.parseAttachResponse({
		ok = true,
		route = "0123456789abcdef",
		project = "C:/p/default.project.json",
		placeKey = "my-level-ab12cd34",
		key = "unpublished-my-level-ab12cd34",
		ephemeral = true,
	})
	expect(ok ~= nil and ok.route == "0123456789abcdef", "a good answer yields the route to connect to")
	expect(ok.placeKey == "my-level-ab12cd34", "...and the key to remember in the place")
	expect(failure == nil and disabled == false, "...with nothing to report")

	local off, offMessage, offDisabled = Broker.parseAttachResponse({ ok = false, disabled = true })
	expect(off == nil and offDisabled == true, "a switched-off app is reported as switched off")
	expect(type(offMessage) == "string", "...with something to say about it")

	local bad, badMessage, badDisabled = Broker.parseAttachResponse({ ok = true, port = 7979 })
	expect(bad == nil and badDisabled == false, "an answer with a port but no route is a failure")
	expect(type(badMessage) == "string", "...and says so")

	expect(Broker.parseAttachResponse("nope") == nil, "and so is an answer that is not an object")

	-- The create request carries the key the place already has, so a project
	-- made later is the one that place keeps finding
	local create = Broker.shapeInitRequest(session, {
		gameId = 42,
		placeId = 7,
		placeName = "My Level",
		placeKey = "my-level-ab12cd34",
	})
	expect(create.placeKey == "my-level-ab12cd34", "a create carries the key the place already carries")

	local unkeyed = Broker.shapeInitRequest(session, { gameId = 42, placeId = 7, placeName = "My Level" })
	expect(unkeyed.placeKey == nil, "...and sends no empty key when the place has none")

	expect(Broker.supportsAttach({ broker = true, projectInit = true, attach = true }), "a hello may advertise attach")
	expect(not Broker.supportsAttach({ broker = true, projectInit = true }), "...and an older app simply does not")
	expect(not Broker.supportsAttach({ attach = true }), "a hello that is not a broker's advertises nothing")
end

print(`\n{checks - failures}/{checks} checks passed`)

if failures > 0 then
	error(`{failures} discovery-policy checks failed`)
end
