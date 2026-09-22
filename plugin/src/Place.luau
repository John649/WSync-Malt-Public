--!strict
-- What the open place is, and the one marker WSync leaves in a place that has
-- no ids to be addressed by (docs/places.md — "Unpublished places").
--
-- A published place is reached by its `placeId`/`gameId`: both sides already
-- share those names, and daemon discovery ranks on them. An **unpublished**
-- place has neither — `game.PlaceId` and `game.GameId` are both `0` — so a
-- daemon can say nothing about it that would distinguish it from every other
-- unpublished place in the world. The marker is that missing name:
--
--   * `ServerStorage/WSyncPlace` — a `StringValue` whose `Value` is the
--     project's `placeKey`. A daemon whose `/hello` reports the same
--     `placeKey` ranks MATCH_EXACT for this place, whatever the ids say.
--   Places marked before the hub may also carry a `DaemonPort` attribute.
--     Nothing reads it any more, and the next `remember` clears it.
--
-- The plugin is the only thing that writes it: the engine never touches
-- Studio, and the desktop app never touches a place. It is written once, after
-- a connect to a project that names a `placeKey` and only when the place does
-- not carry one already, so the key lands in the `.rbxl` on the user's next
-- save and the place finds the same project again next time.
--
-- MARKER EXCLUSION: the same two rules `WSyncBranch` relies on. In `code`
-- scope a `StringValue` is structurally out of `SyncScope.CODE_CLASSES`, so it
-- never reaches disk; for `full` scope the documented `AvoidSync` attribute is
-- stamped on it, which takes it out of sync in every direction. Journal
-- numbering skips it too (`Journal.isExcluded`).

local MarketPlaceService = game:GetService("MarketplaceService")
local ServerStorage = game:GetService("ServerStorage")

local WSync = script:FindFirstAncestor("WSync")

local SyncScope = require(WSync.SyncScope)
local Hex = require(WSync.Transport.Hex)
local generateRef = require(WSync.Helpers.generateRef)

local name = game.Name
local isFile = (name:find(".rbxl") or name:find(".rbxlx")) ~= nil

-- Edit-mode `game.Name` for a published place is Studio's internal place-file
-- name ("Place7"), not what the creator titled it; MarketplaceService is the
-- one API that returns the real name. An unpublished place (PlaceId 0) has
-- nothing to look up, so its `game.Name` stands.
if not isFile and game.PlaceId ~= 0 then
	pcall(function()
		name = MarketPlaceService:GetProductInfo(game.PlaceId).Name
	end)
end

local Place = {}

Place.Name = name
Place.IsFile = isFile
Place.Version = game.PlaceVersion

Place.MARKER_NAME = "WSyncPlace"
Place.MARKER_CLASS = "StringValue"
Place.DAEMON_PORT_ATTRIBUTE = "DaemonPort"

--- Longest key this will read or write. The desktop app mints far shorter
--- ones; the bound is here so a hand-edited marker cannot become a wire field
--- of arbitrary size.
Place.MAX_KEY_LENGTH = 96

--- This Studio window's identity for the life of the session. It is what the
--- desktop app's attach route mixes into a minted key, so two windows on two
--- copies of the same unnamed baseplate never claim one scratch project, and
--- so the *same* window asking twice lands on the project it already has.
Place.SessionId = Hex.encode(generateRef())

--- The marker instance, or nil. Strict, exactly as `Branch.marker` is: the
--- right name, the right class, and directly under `ServerStorage`.
function Place.marker(): Instance?
	local found = ServerStorage:FindFirstChild(Place.MARKER_NAME)

	if found ~= nil and found.ClassName == Place.MARKER_CLASS then
		return found
	end

	return nil
end

--- Is this instance the place marker? The journal and the recorder must never
--- see it.
function Place.isMarker(instance: Instance): boolean
	return instance == Place.marker()
end

local function usableKey(value: any): string?
	if type(value) ~= "string" then
		return nil
	end

	local trimmed = string.match(value, "^%s*(.-)%s*$") :: string

	if trimmed == "" or #trimmed > Place.MAX_KEY_LENGTH then
		return nil
	end

	return trimmed
end

--- The `placeKey` this place carries, when it carries one. Never throws.
function Place.key(): string?
	local ok, key = pcall(function(): string?
		local marker = Place.marker()

		if marker == nil then
			return nil
		end

		return usableKey((marker :: any).Value)
	end)

	return if ok then key else nil
end

--- Write the marker for `key`, unless this place already carries a different
--- one.
---
--- Returns true when the marker now holds `key` — whether this call created it
--- or an earlier one did. Never throws, and never overwrites a key that is
--- already there: a place routed to one project must not be silently
--- re-pointed at another by a connect to a second.
--- Drop the marker, if there is one. Used when a temporary attach is being
--- turned into a real project: the key on the place names the throwaway
--- daemon, and leaving it there would let the post-disconnect rescan find
--- that daemon again instead of the project about to be created. The new
--- project's key is written back on the next connect.
function Place.forget(): boolean
	local ok, removed = pcall(function(): boolean
		local marker = Place.marker()

		if marker == nil then
			return false
		end

		marker:Destroy()

		return true
	end)

	return ok and removed
end

function Place.remember(key: string): boolean
	local wanted = usableKey(key)

	if wanted == nil then
		return false
	end

	local ok, result = pcall(function(): boolean
		local marker = Place.marker()

		if marker ~= nil then
			local existing = usableKey((marker :: any).Value)

			if existing ~= nil and existing ~= wanted then
				-- Somebody else's key. Leave it exactly as it is.
				return false
			end

			if existing == nil then
				(marker :: any).Value = wanted
			end
		else
			local created = Instance.new(Place.MARKER_CLASS)

			created.Name = Place.MARKER_NAME
			created.Value = wanted
			created.Parent = ServerStorage

			marker = created
		end

		local found = marker :: Instance

		-- Full scope has no structural exclusion for a StringValue; the
		-- documented attribute is the marker's own opt-out (SyncScope).
		found:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)

		-- A port remembered before the hub named a daemon by where it happened
		-- to listen, which is exactly what went stale
		if found:GetAttribute(Place.DAEMON_PORT_ATTRIBUTE) ~= nil then
			found:SetAttribute(Place.DAEMON_PORT_ATTRIBUTE, nil)
		end

		return true
	end)

	return ok and result == true
end

return Place
