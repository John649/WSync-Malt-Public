-- Daemon discovery (Design §9.4 / §3.3, amended for the hub) and the
-- desktop-broker Create Project flow (Design §9.2 as amended).
--
-- Every daemon on the machine registers with the WSync hub on
-- `127.0.0.1:7960` (the engine's src/hub), and the hub's `GET /daemons`
-- lists them all with their live `/hello`. So a scan is one request, not a
-- sweep of a port range: the Settings `Host:Port` is asked first (a hub there
-- lists its daemons, a daemon there is a candidate of its own), then the
-- local hub, and the answers are ranked against the open place:
--
--   * published place  — only daemons whose `gameId` equals `game.GameId`
--                        (preferring one whose `placeIds` include
--                        `game.PlaceId`; a claimless daemon never matches)
--   * marked place      — whatever its ids, a daemon whose project names the
--                        same `placeKey` as the place's `ServerStorage/
--                        WSyncPlace` marker matches exactly (docs/places.md)
--   * branch clone      — only the daemon that names the clone's branch
--   * unpublished place — the first responding daemon with no gameId claim
--
-- A daemon found through the hub is connected to through it as well, at its
-- route `/p/<route>`. The route names the project, not the process, so a
-- daemon that restarts on another port is still at the same address.
--
-- `AutoDiscover = false` asks the Settings address alone (pure manual mode).
-- Ranking and the connection state machine live in `Policy` (require-free,
-- standalone-testable); this module owns the HTTP side and the Roblox
-- globals.
--
-- Create Project: when nothing matches, App asks the hub for the desktop
-- app's broker (`findBroker`), which registers with the hub and is reached
-- at `/broker`. A broker hello (`{broker: true, projectInit: true}`)
-- surfaces the explicit Create Project flow — never automatic. For a NEW game
-- it is the two-step confirm; when the hello's `games` list already names
-- this GameId (`brokerKnowsGame`) the button reads "Serve Project" and one
-- click POSTs straight away. Either way the POST carries place *metadata
-- only* (never a filesystem path) to `/projects/init`, then `waitForDaemon`
-- scans again with bounded, cancellable retries until the project's daemon
-- answers.
--
-- Attach (docs/places.md): when the broker also advertises `attach: true`,
-- the no-daemon case does not stop at an offer. `attachPlace` asks for a
-- throwaway daemon for the open place and connects to the route it answers
-- with — no click, nothing written to disk, and the whole thing gone when the
-- window closes. The route is trusted for this session alone
-- (`sessionRoute`), because a scratch daemon for an unpublished place claims
-- no ids that any ranking rule could recognise it by.

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)

local Types = require(WSync.Types)
local Transport = require(WSync.Transport)
local Config = require(WSync.Config)
local Broker = require(WSync.Broker)
local Branch = require(WSync.Branch)
local Place = require(WSync.Place)
local Log = require(WSync.Log)
local Hex = require(WSync.Transport.Hex)
local generateRef = require(WSync.Helpers.generateRef)

local Http = require(WSync.Transport.Http)

local Policy = require(script.Policy)

export type Match = {
	host: string,
	port: number,
	--- `/p/<route>` through the hub, empty for a daemon on its own port
	base: string,
	hello: Types.DaemonHello,
}

export type BrokerMatch = {
	host: string,
	port: number,
	--- Where the broker's routes hang off: `/broker` on the hub
	base: string,
	hello: { [string]: any },
}

local Discovery = {}

Discovery.Policy = Policy
Discovery.HUB_PORT = Policy.HUB_PORT

--- Where this scan looks, from the live Settings values (Policy owns the
--- ordering).
local function sources(): { Policy.Source }
	return Policy.sources({
		host = tostring(Config:get("Host")),
		port = tonumber(Config:get("Port")) or Config:getDefault("Port"),
		https = Config:get("Https") == true,
		autoDiscover = Config:get("AutoDiscover") == true,
	})
end

--- The hub route the desktop app's attach route handed THIS Studio window.
--- Session state on purpose: it is never written into the place, so it
--- cannot outlive the window or reach another one, and a daemon behind it is
--- only trusted while this value stands (`Policy.rankHello`).
Discovery.sessionRoute = nil :: string?

--- Remember (or forget, with nil) the attach route for this session.
function Discovery.setSessionRoute(route: string?)
	Discovery.sessionRoute = route
end

--- The open place as the ranking rules see it. A branch clone is whatever
--- `Branch` says it is; everything else may additionally carry a `placeKey`
--- marker, which is the only name an unpublished place has.
function Discovery.identity(): Policy.PlaceIdentity
	local identity = Branch.current()

	return {
		gameId = game.GameId,
		placeId = game.PlaceId,
		branch = identity ~= nil,
		branchName = if identity ~= nil then identity.name else nil,
		-- The marker rules for a branch clone are untouched: a clone ranks on
		-- its branch name and nothing else, so a place key is not even read
		placeKey = if identity == nil then Place.key() else nil,
		sessionRoute = if identity == nil then Discovery.sessionRoute else nil,
	}
end

--- `GET` a JSON document, or nil on any failure
local function getJson(url: string): any
	return Http.getJson(url)
		:andThen(function(response)
			local ok, decoded = pcall(function()
				return response:decode()
			end)

			return if ok then decoded else nil
		end)
		:catch(function()
			return nil
		end)
		:expect()
end

local function origin(source: Policy.Source): string
	return `{if source.https then "https" else "http"}://{source.host}:{source.port}`
end

--- Ask one address what it is, and what it can reach: a hub answers with
--- every daemon it routes to, a daemon with itself
local function ask(source: Policy.Source): ({ Policy.Candidate }, { Types.DaemonHello })
	local hello = getJson(`{origin(source)}/hello`)

	if Policy.isHubHello(hello) then
		local listing = getJson(`{origin(source)}/daemons`)
		local candidates, hellos = Policy.fromListing(listing, source)

		Log.debug(`Discovery: the hub on {source.host}:{source.port} routes to {#candidates} daemon(s)`)

		return candidates, hellos :: any
	end

	if type(hello) == "table" and type(hello.protocol) == "number" then
		return {
			{
				host = source.host,
				port = source.port,
				https = source.https,
				base = "",
			},
		}, { hello :: Types.DaemonHello }
	end

	return {}, {}
end

--- Find the daemon for the open place. Resolves a `Match` or nil; never
--- rejects. `shouldContinue` aborts the scan early (e.g. on cancel).
---
--- On a branch clone a daemon may still be registering while Studio loads the
--- clone (large clones queue localhost requests behind asset loading), so a
--- clone keeps asking for up to `BRANCH_SCAN_DEADLINE_SECONDS` before giving
--- up; anything else is answered by the first pass.
Discovery.BRANCH_SCAN_DEADLINE_SECONDS = 30

function Discovery.scan(shouldContinue: (() -> boolean)?): Promise.TypedPromise<Match?>
	local function continuing(): boolean
		return not shouldContinue or shouldContinue()
	end

	return Promise.new(function(resolve)
		local place = Discovery.identity()
		local deadline = os.clock() + (if place.branch then Discovery.BRANCH_SCAN_DEADLINE_SECONDS else 0)

		while true do
			local candidates: { Policy.Candidate } = {}
			local hellos: { Types.DaemonHello } = {}

			for _, source in sources() do
				if not continuing() then
					return resolve(nil)
				end

				local found, answers = ask(source)

				for index, candidate in found do
					table.insert(candidates, candidate)
					table.insert(hellos, answers[index])
				end
			end

			if not continuing() then
				return resolve(nil)
			end

			local best = Policy.bestIndex(hellos :: any, #candidates, place, candidates)

			if best ~= nil then
				local candidate = candidates[best]

				return resolve({
					host = candidate.host,
					port = candidate.port,
					base = candidate.base,
					hello = hellos[best],
				})
			end

			if os.clock() >= deadline then
				return resolve(nil)
			end

			task.wait(1)
		end
	end)
end

--------------------------------------------------------------------------------
-- Desktop broker (Create Project)
--------------------------------------------------------------------------------

--- Whether the broker's hello `games` list names the open place's GameId —
--- the app already has a project for this game, so `/projects/init` will
--- adopt and serve it (the broker's existing-gameId rule) rather than create
--- anything. An unpublished place (GameId 0) is never "known": zero is not an
--- identity two places can safely share.
function Discovery.brokerKnowsGame(hello: { [string]: any }): boolean
	if game.GameId == 0 then
		return false
	end

	local games = hello.games

	if type(games) ~= "table" then
		return false
	end

	for _, id in games do
		if id == game.GameId then
			return true
		end
	end

	return false
end

--- Ask the hub for the desktop app's broker. Resolves a `BrokerMatch` when
--- one is registered and answers `{broker: true, projectInit: true}`, nil
--- otherwise; never rejects.
function Discovery.findBroker(shouldContinue: (() -> boolean)?)
	return Promise.new(function(resolve)
		for _, source in sources() do
			if shouldContinue and not shouldContinue() then
				return resolve(nil)
			end

			local listing = getJson(`{origin(source)}/daemons`)
			local broker = if type(listing) == "table" then listing.broker else nil

			if type(broker) == "table" and Broker.isBrokerHello(broker.hello) then
				Log.debug(`Discovery: project-init broker registered with the hub on port {source.port}`)

				return resolve({
					host = source.host,
					port = source.port,
					base = if type(broker.base) == "string" then broker.base else "/broker",
					hello = broker.hello :: { [string]: any },
				})
			end
		end

		return resolve(nil)
	end)
end

--- The place metadata for a `/projects/init` request — ids and names only,
--- never a filesystem path.
local function placeInfo(): Broker.PlaceInfo
	local creatorType: string? = nil
	local creatorId: number? = nil

	local ok, creatorTypeName, creatorIdValue = pcall(function()
		return game.CreatorType.Name, game.CreatorId
	end)

	if ok then
		creatorType = creatorTypeName
		creatorId = creatorIdValue
	end

	-- The published place name. Studio's edit-mode `game.Name` is the internal
	-- place-file name ("Place3"), not what the creator titled the place, so ask
	-- MarketplaceService — the one API that returns the real name in edit mode.
	-- It yields (web call) and is only reachable for a published place; fall
	-- back to `game.Name` for an unpublished one.
	local placeName: string? = if game.Name ~= "" then game.Name else nil

	if game.PlaceId ~= 0 then
		local infoOk, info = pcall(function()
			return game:GetService("MarketplaceService"):GetProductInfo(game.PlaceId)
		end)

		if infoOk and type(info) == "table" and type(info.Name) == "string" and info.Name ~= "" then
			placeName = info.Name
		end
	end

	return {
		gameId = game.GameId,
		placeId = game.PlaceId,
		gameName = nil,
		placeName = placeName,
		creatorType = creatorType,
		creatorId = creatorId,
		-- A place already routed by a key keeps it: the project the app is
		-- about to create claims the same key, so the marker already in the
		-- place points at it from the next connect on (docs/places.md)
		placeKey = Place.key(),
	}
end

--- `POST /projects/init` to the broker with a fresh random request id and
--- the open place's metadata. Resolves `{slug, status}` (`Broker.InitOk`);
--- rejects with a human-readable message on any failure (unreachable broker
--- included).
function Discovery.createProject(broker: BrokerMatch)
	return Promise.new(function(resolve, reject)
		local body = Broker.shapeInitRequest(Hex.encode(generateRef()), placeInfo())

		if body == nil then
			return reject("could not shape the project request from this place's metadata")
		end

		local responseOk, response = pcall(function()
			return Http.postJson(`http://{broker.host}:{broker.port}{broker.base}/projects/init`, body):expect()
		end)

		if not responseOk then
			return reject("the WSync app stopped answering — is it still running?")
		end

		local decodedOk, decoded = pcall(function()
			return response:decode()
		end)

		if not decodedOk then
			return reject("the WSync app answered with an unreadable response")
		end

		local result, failure = Broker.parseInitResponse(decoded)

		if result == nil then
			return reject(failure or "the WSync app rejected the request")
		end

		Log.info(`Create Project: '{result.slug}' ({result.status})`)

		return resolve(result)
	end)
end

--- One daemon's `/hello`, for an address already known. The card uses it to
--- ask the daemon it is connected to whether it is a throwaway, which is a
--- fact of the daemon rather than of how this window reached it.
function Discovery.hello(host: string, port: number, base: string?)
	return Transport.hello(host, port, false, base)
end

--- `POST /places/attach` to the broker for the open place (docs/places.md).
--- Resolves one table — `{attached}` on success, `{message, disabled}` when
--- the app refused or has the flow switched off. Never rejects: a refusal is
--- a fall-back to the ordinary no-daemon card, not an error page.
function Discovery.attachPlace(broker: BrokerMatch)
	return Promise.new(function(resolve)
		local body = Broker.shapeAttachRequest(placeInfo(), Place.SessionId)

		if body == nil then
			return resolve({ message = "could not describe this place to the WSync app" })
		end

		local responseOk, response = pcall(function()
			return Http.postJson(`http://{broker.host}:{broker.port}{broker.base}/places/attach`, body):expect()
		end)

		if not responseOk then
			return resolve({ message = "the WSync app stopped answering — is it still running?" })
		end

		local decodedOk, decoded = pcall(function()
			return response:decode()
		end)

		if not decodedOk then
			return resolve({ message = "the WSync app answered with an unreadable response" })
		end

		local attached, failure, disabled = Broker.parseAttachResponse(decoded)

		if attached == nil then
			return resolve({ message = failure or "the WSync app refused the attach", disabled = disabled })
		end

		Log.info(`Attach: temporary daemon at route {attached.route}`)

		return resolve({ attached = attached })
	end)
end

--- Scan again until a matching daemon appears: bounded
--- (`Broker.WAIT_ATTEMPTS` scans, `Broker.WAIT_DELAY_SECONDS` apart) and
--- cancellable via `shouldContinue`. Resolves the `Match` or nil when the
--- attempts run out or the wait is cancelled. `onAttempt` reports progress.
function Discovery.waitForDaemon(onAttempt: ((attempt: number, total: number) -> ())?, shouldContinue: (() -> boolean)?)
	local function continuing(): boolean
		return not shouldContinue or shouldContinue()
	end

	return Promise.new(function(resolve)
		for attempt = 1, Broker.WAIT_ATTEMPTS do
			if not continuing() then
				return resolve(nil)
			end

			if onAttempt then
				onAttempt(attempt, Broker.WAIT_ATTEMPTS)
			end

			local match = Discovery.scan(shouldContinue):expect()

			if match ~= nil then
				return resolve(match)
			end

			if attempt < Broker.WAIT_ATTEMPTS then
				task.wait(Broker.WAIT_DELAY_SECONDS)
			end
		end

		return resolve(nil)
	end)
end

return Discovery
