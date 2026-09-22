--!strict
-- Branch identity (docs/branches.md — "Discovery" and "Merge in Studio").
--
-- A branch Studio is an ordinary Studio open on a *clone* of the real place.
-- Three things follow from that and live here:
--
--   * Detection — the clone carries exactly one marker, a `StringValue` named
--     `WSyncBranch` directly under `ServerStorage` whose `Value` is the branch
--     name. It is the only instance wsync ever adds to a place, and it is
--     excluded from the journal numbering (Journal.isExcluded) and from disk
--     sync (see MARKER EXCLUSION below).
--
--   * Identity — the clone claims a synthetic `placeId` derived from the
--     branch name, so the branch's daemon (whose project file claims the same
--     id) ranks MATCH_EXACT while main's daemon ranks weak. `Discovery/Policy`
--     then refuses anything below exact for a branch, so a clone that finds no
--     daemon claiming its id never falls through to main.
--
--   * Persistence — the serialized clone and journal preserve branch state.
--     `branch finish` merges it into main. Studio's native File > Save and
--     File > Publish remain outside plugin control.
--
-- MARKER EXCLUSION: in `code` scope the marker is excluded structurally —
-- `StringValue` is not in `SyncScope.CODE_CLASSES`, so it is never written to
-- disk and never deleted from Studio. For `full` scope `apply` stamps the
-- documented `AvoidSync` attribute on the marker, which takes it (and anything
-- under it) out of sync in every direction.
--
-- CONTRACT WITH THE ENGINE (the Rust side writes the marker):
--   * `ServerStorage/WSyncBranch` — StringValue, `Value` = branch name.
--   * optional number attribute `GameId` — the real place's universe id. A
--     clone opened from an `.rbxl` reports `game.GameId == 0`, so the id has
--     to travel on the marker; `apply` falls back to the live `game.GameId`
--     when the attribute is absent.
--   A clone finds its daemon through the hub by the branch name alone: only
--   a daemon whose `/hello` names the same branch ranks at all. Clones made
--   before the hub also carry a `DaemonPort` attribute; nothing reads it.

local ServerStorage = game:GetService("ServerStorage")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local SyncScope = require(WSync.SyncScope)

local Branch = {}

Branch.MARKER_NAME = "WSyncBranch"
Branch.MARKER_CLASS = "StringValue"
Branch.GAME_ID_ATTRIBUTE = "GameId"
Branch.PLACE_ID_ATTRIBUTE = "PlaceId"

export type Identity = {
	name: string,
	syntheticPlaceId: number,
}

--- FNV-1a, 64-bit, over the UTF-8 bytes of the branch name, masked to 53 bits
--- (`hash & (2^53 - 1)`) so the result survives a Lua number and a JSON round
--- trip exactly. The Rust side implements the same function; do not change one
--- without the other.
---
---   basis = 0xCBF29CE484222325
---   prime = 0x00000100000001B3
---   for each byte b:  hash = (hash XOR b) * prime  (mod 2^64)
---   placeId = hash & 0x001FFFFFFFFFFFFF
---
--- Luau numbers are doubles, so the 64-bit state is carried as two 32-bit
--- halves and the prime is split the same way: `phi = 0x00000100`,
--- `plo = 0x000001B3` (the prime is 2^40 + 2^8 + 0xB3). The largest partial
--- sum below is `0xFFFFFFFF * 0x1B3 + 0xFFFFFFFF * 0x100 + 0x1B3` ≈ 2.97e12,
--- comfortably under 2^53, so no product loses a bit.
function Branch.syntheticPlaceId(name: string): number
	local hi = 0xCBF29CE4
	local lo = 0x84222325

	local plo = 0x1B3
	local phi = 0x100

	for index = 1, #name do
		lo = bit32.bxor(lo, string.byte(name, index))

		local low = lo * plo
		local carry = math.floor(low / 0x100000000)

		local nextLo = low % 0x100000000
		local nextHi = (hi * plo + lo * phi + carry) % 0x100000000

		hi = nextHi
		lo = nextLo
	end

	-- 53 bits = the low 21 bits of the high half, then the whole low half
	return (hi % 0x200000) * 0x100000000 + lo
end

--- The marker instance, or nil. Strict: the right name, the right class, and
--- directly under `ServerStorage` — nothing else is the marker.
function Branch.marker(): Instance?
	local found = ServerStorage:FindFirstChild(Branch.MARKER_NAME)

	if found ~= nil and found.ClassName == Branch.MARKER_CLASS then
		return found
	end

	return nil
end

--- Is this instance the branch marker? Used by the journal numbering and the
--- recorder, which must never see it.
function Branch.isMarker(instance: Instance): boolean
	return instance == Branch.marker()
end

--- `{name, syntheticPlaceId}` when the open DataModel is a branch clone, nil
--- when it is main (or any ordinary place). Never throws.
function Branch.detect(_target: DataModel?): Identity?
	local ok, identity = pcall(function(): Identity?
		local marker = Branch.marker()

		if marker == nil then
			return nil
		end

		local name = (marker :: any).Value

		if type(name) ~= "string" or name == "" then
			return nil
		end

		return {
			name = name,
			syntheticPlaceId = Branch.syntheticPlaceId(name),
		}
	end)

	if not ok then
		return nil
	end

	return identity
end

local applied: Identity? = nil

--- Stamp the clone's identity: the real universe id (so a published place's
--- gameId still ranks) and the synthetic placeId the branch daemon claims.
--- Runs at most once per Studio session and swallows every failure — a Studio
--- build without the plugin-security `SetUniverseId`/`SetPlaceId` still boots,
--- it just will not find the branch daemon (Policy then refuses to connect,
--- which is the safe outcome).
function Branch.apply(identity: Identity?): Identity?
	if identity == nil then
		return nil
	end

	if applied ~= nil then
		return applied
	end

	applied = identity

	local marker = Branch.marker()

	if marker ~= nil then
		-- Full scope has no structural exclusion for a StringValue; the
		-- documented attribute is the marker's own opt-out.
		pcall(function()
			marker:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)
		end)
	end

	local universeId = game.GameId

	if marker ~= nil then
		local okAttr, stamped = pcall(function()
			return marker:GetAttribute(Branch.GAME_ID_ATTRIBUTE)
		end)

		if okAttr and type(stamped) == "number" and stamped > 0 then
			universeId = stamped
		end
	end

	-- Only when there is a real universe to claim. A clone opened from a local
	-- `.rbxl` has no universe id and needs none: Discovery matches a branch on
	-- the synthetic placeId and on the daemon's `branch` field, neither of
	-- which involves gameId. Calling SetUniverseId(0) would be a write with
	-- nothing to write.
	if universeId > 0 then
		local okUniverse, err = pcall(function()
			(game :: any):SetUniverseId(universeId)
		end)

		if not okUniverse then
			Log.warn(`Branch '{identity.name}': SetUniverseId failed — {tostring(err)}`)
		end
	end

	-- The REAL place id, stamped on the marker by the assembler: the clone
	-- then answers to the same place as main, so DataStores, MessagingService
	-- and every place-keyed API behave exactly as they do there, and the branch
	-- is told apart by the marker's Value alone. A marker without it (a clone
	-- assembled by an older CLI) keeps the synthetic id. NOTE: with the real
	-- id, File > Publish from a clone window targets the live place.
	local placeId = identity.syntheticPlaceId

	if marker ~= nil then
		local okPlaceAttr, stampedPlace = pcall(function()
			return marker:GetAttribute(Branch.PLACE_ID_ATTRIBUTE)
		end)

		if okPlaceAttr and type(stampedPlace) == "number" and stampedPlace > 0 then
			placeId = stampedPlace
		end
	end

	local okPlace, placeErr = pcall(function()
		(game :: any):SetPlaceId(placeId)
	end)

	if not okPlace then
		Log.warn(`Branch '{identity.name}': SetPlaceId failed — {tostring(placeErr)}`)
	end

	local universe = if universeId > 0 then tostring(universeId) else "unset (local file)"

	Log.stage(`Branch '{identity.name}': placeId {placeId}, universe {universe}`)

	return applied
end

--- The identity `apply` stamped, or nil on main. Every other module asks this
--- rather than re-detecting (detection walks ServerStorage).
function Branch.current(): Identity?
	return applied
end

function Branch.isBranch(): boolean
	return applied ~= nil
end

return Branch
