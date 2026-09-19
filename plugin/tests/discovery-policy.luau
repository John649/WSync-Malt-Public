-- Discovery's pure rules for place routing and the automatic attach
-- (docs/places.md). Runs the real `Discovery/Policy` and `Broker` modules —
-- both require-free — under standalone luau; `ref` and `require` arrive from
-- harness/loader.luau.
--
-- What is covered: the `placeKey` marker rank that makes an unpublished place
-- routable at all, the session-port hint an attach is trusted by, the branch
-- rules staying untouched by both, the candidate ordering those hints imply,
-- the attach states in the connection machine, and the request/response shapes
-- the attach route is spoken to with.

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

local function at(port: number): Policy.Candidate
	return { host = "127.0.0.1", port = port, https = false }
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
print("the session-port hint")
--------------------------------------------------------------------------------

do
	local attached = place({ sessionPort = 7979 })

	expect(
		Policy.rankHello({}, attached, at(7979)) == Policy.MATCH_EXACT,
		"the port the app handed this window is exact, though the daemon claims nothing"
	)

	expect(
		Policy.rankHello({}, attached, at(7980)) == Policy.MATCH_WEAK,
		"...and only that port: every other claimless daemon ranks as it always did"
	)

	expect(
		Policy.rankHello({}, attached) == Policy.MATCH_WEAK,
		"a caller that does not say where the hello came from cannot take the hint"
	)

	expect(
		Policy.rankHello({}, attached, { host = "example.com", port = 7979, https = false }) == Policy.MATCH_WEAK,
		"the hint is a loopback port, not a port number anywhere in the world"
	)

	expect(
		Policy.rankHello({ branch = "ui-pass" }, attached, at(7979)) == Policy.MATCH_NONE,
		"the mirror rule still wins: an unmarked window never binds to a branch daemon"
	)

	expect(
		Policy.rankHello({ gameId = 999 }, place({ gameId = 42, sessionPort = 7979 }), at(7979)) == Policy.MATCH_EXACT,
		"a published place that asked for an attach trusts the port it was given"
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
		sessionPort = 7979,
	})

	expect(
		Policy.rankHello({ branch = "ui-pass" }, clone, at(7990)) == Policy.MATCH_EXACT,
		"a clone still matches the daemon that names its branch"
	)

	expect(
		Policy.rankHello({ placeKey = "my-level-ab12cd34" }, clone, at(7979)) == Policy.MATCH_NONE,
		"neither a matching key nor the session port reaches a clone: only its branch does"
	)

	expect(
		Policy.rankHello({ gameId = 0, placeIds = {} }, clone, at(7979)) == Policy.MATCH_NONE,
		"...and a clone whose branch daemon is absent still refuses rather than falling through"
	)
end

--------------------------------------------------------------------------------
print("candidate ordering")
--------------------------------------------------------------------------------

do
	local list = Policy.candidates({
		host = "127.0.0.1",
		port = 7985,
		autoDiscover = true,
		sessionPort = 7979,
		branchPort = 7981,
		markerPort = 7982,
	})

	expect(list[1].port == 7979, "the session port is probed first")
	expect(list[2].port == 7981, "then the branch port")
	expect(list[3].port == 7982, "then the marker port")
	expect(list[4].port == 7985, "then the Settings address")

	local seen: { [number]: number } = {}

	for _, candidate in list do
		seen[candidate.port] = (seen[candidate.port] or 0) + 1
	end

	local duplicated = false

	for _, count in seen do
		if count > 1 then
			duplicated = true
		end
	end

	expect(not duplicated, "no port is probed twice, however many hints name it")
	-- Every hint here, and the Settings port, already fall inside the scan
	-- range: so the whole list is that range, each port exactly once
	expect(
		#list == Policy.PORT_MAX - Policy.PORT_MIN + 1,
		"hints inside the scan range reorder it rather than lengthening it"
	)

	-- A hint that IS the Settings address is not a second candidate
	local same = Policy.candidates({ host = "localhost", port = 7979, autoDiscover = false, sessionPort = 7979 })

	expect(#same == 1 and same[1].port == 7979, "a hint equal to the Settings address collapses into one probe")

	local none = Policy.candidates({ host = "10.0.0.5", port = 9000, autoDiscover = false })

	expect(#none == 1 and none[1].host == "10.0.0.5", "manual mode is still the Settings candidate alone")
end

--------------------------------------------------------------------------------
print("the attach states")
--------------------------------------------------------------------------------

do
	local state = Policy.initialState()
	local effect

	state, effect = Policy.step(state, { kind = "start" })
	expect(state.name == "scanning" and effect == "scan", "start scans")

	local broker = { host = "127.0.0.1", port = 7968 }
	local scanning = state

	state, effect = Policy.step(state, { kind = "scan-none-attach", generation = state.generation, broker = broker })
	expect(state.name == "attaching" and effect == "attach", "no daemon plus an attach-capable broker attaches")
	expect(state.broker == broker, "the broker rides along so the request knows where to go")

	local attaching = state

	state, effect = Policy.step(state, {
		kind = "attach-ok",
		generation = state.generation,
		host = "127.0.0.1",
		port = 7979,
		project = "unpublished-my-level-ab12cd34",
	})
	expect(state.name == "found" and effect == "connect", "the answer connects straight to the port")
	expect(state.port == 7979 and state.temporary == true, "...and the connection is marked temporary")

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

	local temporary = Policy.describe({ name = "found", generation = 1, port = 7979, temporary = true }, env)
	expect(string.find(temporary.status, "Attached temporarily") ~= nil, "a temporary connect says so while connecting")

	local ordinary = Policy.describe({ name = "found", generation = 1, port = 7979, project = "dungeon" }, env)
	expect(string.find(ordinary.status, "Found 'dungeon'") ~= nil, "an ordinary connect is unchanged")

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
		port = 7979,
		project = "C:/p/default.project.json",
		placeKey = "my-level-ab12cd34",
		key = "unpublished-my-level-ab12cd34",
		ephemeral = true,
	})
	expect(ok ~= nil and ok.port == 7979, "a good answer yields the port to connect to")
	expect(ok.placeKey == "my-level-ab12cd34", "...and the key to remember in the place")
	expect(failure == nil and disabled == false, "...with nothing to report")

	local off, offMessage, offDisabled = Broker.parseAttachResponse({ ok = false, disabled = true })
	expect(off == nil and offDisabled == true, "a switched-off app is reported as switched off")
	expect(type(offMessage) == "string", "...with something to say about it")

	local bad, badMessage, badDisabled = Broker.parseAttachResponse({ ok = true, port = 0 })
	expect(bad == nil and badDisabled == false, "an answer with no usable port is a failure")
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
