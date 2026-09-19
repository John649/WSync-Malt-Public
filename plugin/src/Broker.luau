--!strict
-- Pure rules for the desktop-broker Create Project flow (Design §9.2 as
-- amended; pinned wave-5 contract). The broker is the desktop app's helper
-- listening on `127.0.0.1:7968-7971`; its `GET /hello` answers
-- `{broker: true, projectInit: true}`, and `POST /projects/init` creates a
-- project from place *metadata only* — never a filesystem path.
--
-- `POST /places/attach` is the second route (docs/places.md — "Attach from
-- Studio"): a place with no daemon is handed a throwaway one, with no click
-- and nothing written to disk, and the answer is the port to connect to. It
-- may also answer `{disabled: true}`, which is not a failure — it is the
-- user having switched the flow off, and the caller falls back to the
-- ordinary no-daemon card.
--
-- This module owns everything about those exchanges that does not need a
-- socket: hello recognition, request shaping, response parsing, and the
-- bounded daemon-wait schedule. Discovery orchestrates the HTTP side. No
-- requires, no Roblox APIs — loadable under a standalone `luau` binary for
-- behavior tests (the WriteRules pattern).

local Broker = {}

Broker.PORT_MIN = 7968
Broker.PORT_MAX = 7971

-- After a successful init, re-scan the daemon range until the new project's
-- daemon appears: bounded, cancellable (Discovery.waitForDaemon).
Broker.WAIT_ATTEMPTS = 15
Broker.WAIT_DELAY_SECONDS = 2

export type PlaceInfo = {
	gameId: number,
	placeId: number,
	gameName: string?,
	placeName: string?,
	creatorType: string?,
	creatorId: number?,
	--- The key this place already carries on its `WSyncPlace` marker
	--- (docs/places.md). Sent with a create so the new project claims the key
	--- the place is already routed by, instead of the place being orphaned
	--- from it.
	placeKey: string?,
}

export type InitOk = {
	slug: string,
	status: string,
}

--- A daemon the attach route started for this window. `placeKey` is present
--- only for an unpublished place, which is the only kind that needs one.
export type AttachOk = {
	port: number,
	project: string?,
	placeKey: string?,
	key: string?,
}

--- Whether a decoded `/hello` body identifies a project-init-capable broker.
function Broker.isBrokerHello(value: any): boolean
	return type(value) == "table" and value.broker == true and value.projectInit == true
end

--- Whether that broker also serves `POST /places/attach`. Gating on the hello
--- rather than on a 404 keeps an older desktop app from costing a round trip
--- on every place that has no daemon.
function Broker.supportsAttach(hello: any): boolean
	return Broker.isBrokerHello(hello) and hello.attach == true
end

local function isCountingNumber(value: any): boolean
	return type(value) == "number" and value == value and value >= 0 and value < math.huge and value % 1 == 0
end

--- Shape the `POST /projects/init` body: the request id plus place metadata,
--- with empty or unknown fields omitted rather than sent blank. Returns nil
--- when the inputs cannot form a valid request (malformed request id or
--- ids) — the caller treats that as a programming error, not a wire error.
function Broker.shapeInitRequest(requestId: any, place: PlaceInfo): { [string]: any }?
	if type(requestId) ~= "string" or #requestId < 16 or requestId:find("[^0-9a-f]") ~= nil then
		return nil
	end

	if not isCountingNumber(place.gameId) or not isCountingNumber(place.placeId) then
		return nil
	end

	local body: { [string]: any } = {
		requestId = requestId,
		gameId = place.gameId,
		placeId = place.placeId,
	}

	if type(place.gameName) == "string" and place.gameName ~= "" then
		body.gameName = place.gameName
	end

	if type(place.placeName) == "string" and place.placeName ~= "" then
		body.placeName = place.placeName
	end

	if type(place.placeKey) == "string" and place.placeKey ~= "" then
		body.placeKey = place.placeKey
	end

	if
		(place.creatorType == "User" or place.creatorType == "Group")
		and isCountingNumber(place.creatorId)
		and (place.creatorId :: number) > 0
	then
		body.creatorType = place.creatorType
		body.creatorId = place.creatorId
	end

	return body
end

--- Shape the `POST /places/attach` body: the ids when the place has them
--- (both are 0 for an unpublished place, which is the case the route exists
--- for), what the place calls itself, and this Studio session's id — which is
--- what keeps two windows on two copies of one unnamed baseplate apart.
--- Returns nil when the inputs cannot form a valid request.
function Broker.shapeAttachRequest(place: PlaceInfo, sessionId: any): { [string]: any }?
	if type(sessionId) ~= "string" or #sessionId < 8 or sessionId:find("[^0-9a-fA-F]") ~= nil then
		return nil
	end

	if not isCountingNumber(place.gameId) or not isCountingNumber(place.placeId) then
		return nil
	end

	local body: { [string]: any } = {
		placeId = place.placeId,
		gameId = place.gameId,
		sessionId = string.lower(sessionId),
	}

	if type(place.placeName) == "string" and place.placeName ~= "" then
		body.name = place.placeName
	end

	return body
end

--- Parse a `/places/attach` response body. Returns the `{port, …}` on
--- success; nil plus a reason otherwise. `disabled` is the second return's
--- own case: the route answered, the user has the flow switched off, and the
--- caller falls back rather than showing an error.
function Broker.parseAttachResponse(value: any): (AttachOk?, string?, boolean)
	if type(value) ~= "table" then
		return nil, "the WSync app answered with an unreadable response", false
	end

	if value.disabled == true then
		return nil, "automatic attach is switched off in the WSync app", true
	end

	if value.ok ~= true then
		local message = value.error

		if type(message) == "table" then
			message = message.message or message.code
		end

		return nil,
			if type(message) == "string" and message ~= "" then message else "the WSync app refused the attach",
			false
	end

	local port = value.port

	if not isCountingNumber(port) or port <= 0 or port >= 65536 then
		return nil, "the WSync app did not name a port to connect to", false
	end

	return {
		port = port :: number,
		project = if type(value.project) == "string" then value.project else nil,
		placeKey = if type(value.placeKey) == "string" and value.placeKey ~= "" then value.placeKey else nil,
		key = if type(value.key) == "string" and value.key ~= "" then value.key else nil,
	},
		nil,
		false
end

--- Parse a `/projects/init` response body. Returns the `{slug, status}` on
--- `{ok: true, slug, status}`, or nil plus a human-readable reason.
function Broker.parseInitResponse(value: any): (InitOk?, string?)
	if type(value) ~= "table" then
		return nil, "the broker answered with a malformed response"
	end

	if value.ok ~= true then
		local message = value.error

		if type(message) == "table" then
			message = message.message or message.code
		end

		return nil, if type(message) == "string" and message ~= "" then message else "the broker rejected the request"
	end

	if type(value.slug) ~= "string" or value.slug == "" then
		return nil, "the broker did not name the created project"
	end

	return {
		slug = value.slug :: string,
		status = if type(value.status) == "string" and value.status ~= "" then value.status else "created",
	},
		nil
end

return Broker
