--!strict
-- Place file ops (docs/place-file.md). The daemon keeps `place.rbxl` in the
-- project root: this Studio's place as a binary place file, so Lune — or
-- anything else that reads rbx_binary — can open the whole place without
-- Studio. Two ops:
--
--   place_file_status {}           -> {epoch, busy?, services = [{name,
--                                      className, generation, properties}]}
--   place_file_capture {service}   -> {name, generation, roots, blob?}
--
-- and the bytes travel like a branch checkpoint's service blobs: pinned in the
-- journal's transfer store (Journal.captureBlobBytes) and pulled with
-- `branch_blob_open`/`branch_blob_read`/`branch_blob_close`.
--
-- ONE SERVICE PER CALL. SerializeInstancesAsync holds Studio's main thread for
-- as long as it runs — ServerStorage on a 250k-instance place takes ~3 s, the
-- whole place ~9 s — so a single call over every service would freeze Studio
-- past the socket's 8 s heartbeat and cost the connection. Between two
-- captures Studio gets its frames back and the socket its pong.
--
-- FRESHNESS IS COUNTED, NOT DIFFED. The first status call starts a watch —
-- `game.ItemChanged` plus each service's DescendantAdded/DescendantRemoving,
-- a fixed handful of connections rather than one per instance — that marks a
-- service changed whenever anything under it changes. A changed service's
-- generation moves the next time anyone reads it, so the daemon, which keeps
-- the generation it captured each service at, re-captures only the services
-- whose number moved: an edit in Workspace never re-serializes ServerStorage.
-- `epoch` names this watch; generations restart from zero with it, so a daemon
-- holding another epoch's numbers starts over.
--
-- SERVICES THEMSELVES cannot be serialized (SerializationService refuses
-- them), so their own properties — Lighting's ClockTime, Workspace's Gravity,
-- their attributes — travel as encoded values in every status, read fresh.
-- References are the one thing lost: a Ref property on a service is left out,
-- and SerializationService drops any reference that leaves the service it
-- serialized, exactly as a branch checkpoint does.

local HttpService = game:GetService("HttpService")

local WSync = script:FindFirstAncestor("WSync")

local Dom = require(WSync.Lib.Dom)
local Journal = require(WSync.Journal)

local Error = require(script.Parent.Parent.Error)

type Args = { [string]: any }

local PlaceFileOps = {}

--- The services a place file carries: the journal's eleven content services
--- plus the few others whose children are authored content. The daemon writes
--- them in this order.
PlaceFileOps.SERVICES = table.clone(Journal.CONTENT_SERVICES)

for _, name in { "MaterialService", "Teams", "LocalizationService" } do
	table.insert(PlaceFileOps.SERVICES, name)
end

local function invalid(message: string): never
	error(Error.new(Error.INVALID_ARGUMENT, message), 0)
end

local function pluginError(message: string): never
	error(Error.new(Error.PLUGIN_ERROR, message), 0)
end

local function serializationService(): any
	local ok, service = pcall(function()
		return game:GetService("SerializationService" :: any)
	end)

	if not ok or service == nil then
		pluginError("this Studio build does not expose SerializationService")
	end

	return service
end

--------------------------------------------------------------------------------
-- The watch
--------------------------------------------------------------------------------

local epoch: string? = nil
local byInstance: { [Instance]: string } = {}
local byName: { [string]: Instance } = {}
local generations: { [string]: number } = {}
local changed: { [string]: boolean } = {}

--- Mark whichever tracked service `instance` sits under as changed. Called
--- for every property change of every instance in the DataModel — Studio's
--- own plugin cameras fire dozens a second — so the common path is a few
--- `Parent` reads and a table lookup, and the exclusion check runs only for
--- a service that is not already marked.
local function touch(instance: Instance)
	local child: Instance? = nil
	local current = instance

	while true do
		local parent = current.Parent

		if parent == nil then
			return
		end

		if parent == game then
			break
		end

		child = current
		current = parent
	end

	local name = byInstance[current]

	-- A service's own properties are read fresh by every status; only what
	-- is under it needs serializing again
	if name == nil or child == nil or changed[name] then
		return
	end

	-- The workspace camera moves whenever the user looks around, and nothing
	-- Journal.isExcluded names is in the file anyway
	if Journal.isExcluded(child :: Instance, current) then
		return
	end

	changed[name] = true
end

local function startWatch()
	if epoch ~= nil then
		return
	end

	epoch = HttpService:GenerateGUID(false)

	for _, name in PlaceFileOps.SERVICES do
		local ok, service = pcall(function()
			return game:GetService(name :: any)
		end)

		if not ok or typeof(service) ~= "Instance" then
			continue
		end

		byInstance[service] = name
		byName[name] = service
		generations[name] = 0

		service.DescendantAdded:Connect(touch)
		service.DescendantRemoving:Connect(touch)
	end

	pcall(function()
		(game :: any).ItemChanged:Connect(touch)
	end)
end

--- The generation `name` is at now. A change since the last read moves it
--- exactly once, so reading it before a capture and comparing it after is
--- enough to know whether the capture is still current.
local function generationOf(name: string): number
	if changed[name] then
		changed[name] = false
		generations[name] = (generations[name] or 0) + 1
	end

	return generations[name] or 0
end

--------------------------------------------------------------------------------
-- Service properties
--------------------------------------------------------------------------------

--- A value is only as good as its JSON: a non-UTF-8 string anywhere in it
--- would fail the encode of the whole status, not just this property.
local function jsonSafe(value: any): boolean
	local kind = type(value)

	if kind == "string" then
		return utf8.len(value) ~= nil
	end

	if kind == "number" then
		return value == value and value ~= math.huge and value ~= -math.huge
	end

	if kind == "table" then
		for key, inner in value do
			if not jsonSafe(key) or not jsonSafe(inner) then
				return false
			end
		end
	end

	return true
end

--- Every readable property of a service in the daemon's encoding, Ref-typed
--- ones left out: a ref names an instance by an id only this Studio knows.
local function serviceProperties(service: Instance): { [string]: any }
	local properties = {}

	for property, default in Dom.getDefaultProperties(service.ClassName) do
		if next(default) == "Ref" then
			continue
		end

		local encoded = Journal.encodeProperty(service, property)

		if encoded ~= nil and jsonSafe(encoded) then
			properties[property] = encoded
		end
	end

	return properties
end

--- Whether Studio is running a playtest right now. Serializing the edit
--- place freezes the playtest with it, so the daemon's own refresh waits;
--- an explicit `wsync rbxl` does not ask.
local function playtestRunning(): boolean
	local ok, running = pcall(function()
		local service = game:GetService("PluginConnectionService" :: any) :: any

		for _, connection in service:GetPluginConnectionsOfType(Enum.PluginConnectionTargetType.Test) do
			if connection.Connected then
				return true
			end
		end

		return false
	end)

	return ok and running == true
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

PlaceFileOps.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `place_file_status {}` → `{epoch, busy?, services}`, one entry per
--- service present in this place, in file order. Cheap: no serialization,
--- one property read per reflected service property.
function PlaceFileOps.ops.place_file_status(_args: Args, _ctx)
	startWatch()

	local services = {}

	for _, name in PlaceFileOps.SERVICES do
		local service = byName[name]

		if service == nil then
			continue
		end

		table.insert(services, {
			name = name,
			className = service.ClassName,
			generation = generationOf(name),
			properties = serviceProperties(service),
		})
	end

	return {
		epoch = epoch,
		busy = if playtestRunning() then "playtest" else nil,
		services = services,
	}
end

--- `place_file_capture {service}` → `{name, generation, roots, blob?}`.
--- `generation` is read BEFORE the serialize, so an edit that lands while it
--- yields moves the number again and the daemon comes back for it. A service
--- with nothing serializable under it answers `roots = 0` and no blob.
function PlaceFileOps.ops.place_file_capture(args: Args, _ctx)
	startWatch()

	local name = args.service

	if type(name) ~= "string" or name == "" then
		invalid("service must be a service name")
	end

	local service = byName[name]

	if service == nil then
		invalid(`'{name}' is not a service this place file carries (see place_file_status)`)
	end

	local generation = generationOf(name)
	local roots: { Instance } = {}

	for _, child in (service :: Instance):GetChildren() do
		if not Journal.isExcluded(child, service :: Instance) then
			table.insert(roots, child)
		end
	end

	if #roots == 0 then
		return { name = name, generation = generation, roots = 0 }
	end

	local serializer = serializationService()

	local ok, bytes = pcall(function()
		return serializer:SerializeInstancesAsync(roots)
	end)

	if not ok or typeof(bytes) ~= "buffer" then
		pluginError(`could not serialize '{name}': {tostring(bytes)}`)
	end

	local blobId = Journal.captureBlobBytes(bytes :: buffer)

	if blobId == nil then
		pluginError(`the blob store rejected '{name}'`)
	end

	local info = Journal.blobInfo(blobId :: string)

	return {
		name = name,
		generation = generation,
		roots = #roots,
		blob = {
			artifactId = blobId,
			bytes = if info ~= nil then info.bytes else buffer.len(bytes :: buffer),
			sha256 = if info ~= nil then info.sha256 else nil,
		},
	}
end

return PlaceFileOps
