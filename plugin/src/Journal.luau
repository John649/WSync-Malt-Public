--!strict
-- The journal (docs/branches.md — "Principle: track, don't diff").
--
-- A branch does not export its place and let a merge reconstruct what
-- happened; it records what happened. This module is that recorder, and it
-- runs identically on main (where it keeps one stream with a checkpoint mark
-- per branch) and on a branch clone (where it keeps the stream a merge
-- replays). `Journal.replay` is the other half: main's plugin applying a
-- branch's stream against the live tree.
--
-- NUMBERING (wire contract, shared with the engine — do not change one side
-- alone). At a checkpoint every instance under the eleven content services
-- gets the id `"<n>"`, by walking the services in the fixed order below and
-- each instance's children in `GetChildren` order, depth-first (pre-order),
-- skipping — together with everything under them:
--
--   * any instance with `Archivable = false`
--   * any `Camera` directly under `Workspace`
--   * the branch marker (`ServerStorage/WSyncBranch`)
--
-- Both Studios hold identical trees at the checkpoint, so both produce
-- identical numbering. Instances created afterwards get `"new:<m>"`, m a
-- per-session counter. The eleven services themselves are not numbered (they
-- exist in every place); they are addressable as `"svc:<Name>"` so an event
-- about a top-level instance has a parent to name. `id -> Instance` and
-- `Instance -> id` are held strongly for the Studio's lifetime, exactly as the
-- contract requires: an id must keep meaning something after the instance
-- leaves the tree.
--
-- EVENTS (wire contract). `{ t = <unix ms>, seq = <n>, id, op, ... }` with
--
--   add    { parent, name, class, blob? }   blob = a serialized-subtree id
--   remove {}
--   move   { parent }
--   rename { name }
--   set    { prop, value }                  value: the daemon property codec
--   attr   { name, value|null }
--   tag    { name, on }
--
-- plus `rec` — the ChangeHistory recording name — when it is known. Events
-- coalesce per `(id, op, prop)`: the latest wins, a `remove` is never dropped,
-- and an id added and removed before the same flush leaves nothing behind.
--
-- TRANSPORT. Batches go to the connected daemon as
-- `POST /journal/append  { checkpoint, events: [...] }` every
-- `FLUSH_SECONDS` while there is anything pending, and on `journal_flush`.
-- `Core` stamps the address (`Journal.setEndpoint`) when it connects.
--
-- BLOBS. An `add` carries its subtree as an rbxm serialized by
-- SerializationService and pinned in a transfer store exactly the way `copy`
-- pins a clipboard payload (Remote/Transfers). One id space and one store
-- serve both kinds of blob — an `add`'s subtree and a `branch_checkpoint`
-- service blob — and the daemon drains either with
-- `branch_blob_open`/`branch_blob_read`/`branch_blob_close`. Roots still in the
-- tree are re-captured on every flush once their last capture is older than
-- `BLOB_REFRESH_SECONDS`, so a crash loses seconds rather than a subtree.

local CollectionService = game:GetService("CollectionService")
local ChangeHistoryService = game:GetService("ChangeHistoryService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local Dom = require(WSync.Lib.Dom)
local Base64 = require(WSync.Lib.Base64)
local Branch = require(WSync.Branch)
local Hex = require(WSync.Transport.Hex)
local Http = require(WSync.Transport.Http)
local generateRef = require(WSync.Helpers.generateRef)
local BufferHash = require(WSync.Remote.BufferHash)
local Transfers = require(WSync.Remote.Transfers)

type Event = { [string]: any }

local Journal = {}

--- The eleven content services, in the fixed walk order. Numbering depends on
--- this list and its order; the engine carries the same one.
Journal.CONTENT_SERVICES = {
	"Workspace",
	"Lighting",
	"ReplicatedFirst",
	"ReplicatedStorage",
	"ServerScriptService",
	"ServerStorage",
	"StarterGui",
	"StarterPack",
	"StarterPlayer",
	"SoundService",
	"TextChatService",
}

Journal.FLUSH_SECONDS = 2
Journal.BLOB_REFRESH_SECONDS = 10
Journal.SWEEP_SECONDS = 30
Journal.SELECTION_WINDOW_SECONDS = 300
Journal.SWEEP_MAX_INSTANCES = 256
-- A pinned blob lives until `branch_blob_close` drops it, or ten minutes,
-- whichever comes first (the CLI's pull contract).
Journal.BLOB_TTL_SECONDS = 600
Journal.BLOB_READ_MAX_LEN = 512 * 1024

-- Properties the recorder never reports. `Parent` is structure (move events
-- carry it) and `Source` is git's (wsync writes every script to disk as it
-- changes, so the replay must never fight the worktree over one byte).
local SKIP_PROPERTIES: { [string]: boolean } = { Parent = true }

--------------------------------------------------------------------------------
-- State
--------------------------------------------------------------------------------

local active = false
local checkpointId: string? = nil
local checkpointCount = 0

local idOf: { [Instance]: string } = {}
local byId: { [string]: Instance } = {}
local newCounter = 0

local seq = 0
local pending: { [string]: Event } = {}
local pendingKeys: { [string]: { [string]: boolean } } = {}
local pendingCount = 0

-- Every event ever emitted, uncoalesced: main's side of a merge compares a
-- branch's stream against the slice of this one that follows the branch's
-- checkpoint mark.
local history: { Event } = {}
local marks: { [string]: number } = {}

local connections: { RBXScriptConnection } = {}
local watched: { [Instance]: boolean } = {}
local subscribedTags: { [string]: boolean } = {}
local recent: { [Instance]: number } = {}
local snapshots: { [Instance]: { [string]: any } } = {}

type AddedRoot = { instance: Instance, parent: string, name: string, class: string, capturedAt: number }

local addedRoots: { [string]: AddedRoot } = {}
local blobs = Transfers.newOutStore({ ttl = Journal.BLOB_TTL_SECONDS, dropOnEof = false })

local endpoint: { host: string, port: string, https: boolean }? = nil
local recording: string? = nil

local flushedTotal = 0
local lastFlushAt = 0
local looping = false

-- While the recorder is muted it neither emits nor allocates ids. `branch up`
-- replays a branch's own journal into its own clone (`own` mode below), and
-- those changes ARE the journal — recording them again would duplicate every
-- event the next flush pushes.
local muted = false

--------------------------------------------------------------------------------
-- Services, exclusion and the walk
--------------------------------------------------------------------------------

local function serializationService(): any?
	local ok, service = pcall(function()
		return game:GetService("SerializationService" :: any)
	end)

	return if ok then service else nil
end

--- The eleven content services that exist in this DataModel, in walk order.
function Journal.services(): { Instance }
	local list: { Instance } = {}

	for _, name in Journal.CONTENT_SERVICES do
		local ok, service = pcall(function()
			return game:GetService(name :: any)
		end)

		if ok and typeof(service) == "Instance" then
			table.insert(list, service)
		end
	end

	return list
end

--- Is this instance (and therefore its subtree) outside the numbering?
function Journal.isExcluded(instance: Instance, service: Instance): boolean
	local ok, archivable = pcall(function()
		return instance.Archivable
	end)

	if ok and archivable == false then
		return true
	end

	if service.Name == "Workspace" and instance.Parent == service and instance:IsA("Camera") then
		return true
	end

	return Branch.isMarker(instance)
end

local function serviceIdOf(service: Instance): string
	return `svc:{service.Name}`
end

local function assign(instance: Instance, id: string)
	idOf[instance] = id
	byId[id] = instance
end

--- The compact tree shape a branch aligns against (SHAPE_VERSION 1):
---
---   { v = 1, count = <total>, services = {
---       { service = "Workspace", base = <ids before this service>,
---         nodes = { {name, class, childCount}, ... } }, ... } }
---
--- `nodes` is the service's walk in numbering order, so the node at index `k`
--- carries the id `base + k`, and `childCount` is what makes the flat array
--- reconstructible into a tree (pre-order: a node is followed by its whole
--- first subtree, then the next). Written by main in `branch_checkpoint` and
--- read by a clone in `branch_adopt`; the engine only moves the bytes.
Journal.SHAPE_VERSION = 1

export type ShapeService = { service: string, base: number, nodes: { { any } } }
export type Shape = { v: number, count: number, services: { ShapeService } }

--- Number the tree. Every previous id is dropped: a checkpoint is the new
--- origin, and two Studios that number the same tree agree by construction.
--- Returns `{count}`, plus `shape` when `withShape` asks for it — built by
--- the SAME walk that numbers, so the two can never describe different trees.
function Journal.checkpoint(services: { Instance }?, withShape: boolean?): { count: number, shape: Shape? }
	local list = services or Journal.services()

	table.clear(idOf)
	table.clear(byId)
	table.clear(addedRoots)

	newCounter = 0

	local n = 0
	local shape: Shape? = if withShape then { v = Journal.SHAPE_VERSION, count = 0, services = {} } else nil

	--- Number `instance`'s children, recording them into `nodes` when asked.
	--- Returns how many children were numbered (the parent's `childCount`).
	local function collect(instance: Instance, service: Instance, nodes: { { any } }?): number
		local children = 0

		for _, child in instance:GetChildren() do
			if Journal.isExcluded(child, service) then
				continue
			end

			children += 1
			n += 1
			assign(child, tostring(n))

			if nodes == nil then
				collect(child, service, nil)
				continue
			end

			-- Appended before the recursion, so the index IS the pre-order
			-- position, and patched after it, once the count is known.
			local entry = { child.Name, child.ClassName, 0 }
			table.insert(nodes, entry)
			entry[3] = collect(child, service, nodes)
		end

		return children
	end

	for _, service in list do
		byId[serviceIdOf(service)] = service

		if shape == nil then
			collect(service, service, nil)
			continue
		end

		local entry: ShapeService = { service = service.Name, base = n, nodes = {} }

		collect(service, service, entry.nodes)
		table.insert((shape :: Shape).services, entry)
	end

	checkpointCount = n

	if shape ~= nil then
		(shape :: Shape).count = n
	end

	return { count = n, shape = shape }
end

--------------------------------------------------------------------------------
-- Adopting main's numbering through a shape
--------------------------------------------------------------------------------

--- How far ahead the aligner looks for a match before it gives up on the pair
--- in front of it. Bounded on purpose: an unbounded search would be O(n²) on a
--- parent with tens of thousands of children.
Journal.ALIGN_WINDOW = 32

--- The share of main's nodes a clone must match for its adoption to be
--- trusted. Studio adds instances of its own when it opens a place file (a
--- Lighting post-processing child, for one), so an exact count can never be
--- required — but a tree that matches only loosely is not the same tree.
Journal.ALIGN_MIN_RATIO = 0.99

type ShapeNode = { index: number, name: string, class: string, size: number, children: { ShapeNode } }

--- Rebuild a service's flat pre-order array into a forest. Each node keeps its
--- flat index (its id, once `base` is added) and its subtree size (what a
--- skipped node costs in unused ids).
local function parseShapeNodes(nodes: { { any } }): { ShapeNode }
	local cursor = 1

	local function parseNode(): ShapeNode
		local entry = nodes[cursor]
		local node: ShapeNode = {
			index = cursor,
			name = tostring(entry[1]),
			class = tostring(entry[2]),
			size = 1,
			children = {},
		}

		cursor += 1

		for _ = 1, (tonumber(entry[3]) or 0) do
			if cursor > #nodes then
				break
			end

			local child = parseNode()

			table.insert(node.children, child)
			node.size += child.size
		end

		return node
	end

	local roots: { ShapeNode } = {}

	while cursor <= #nodes do
		table.insert(roots, parseNode())
	end

	return roots
end

local function liveChildrenOf(instance: Instance, service: Instance): { Instance }
	local out: { Instance } = {}

	for _, child in instance:GetChildren() do
		if not Journal.isExcluded(child, service) then
			table.insert(out, child)
		end
	end

	return out
end

type AlignStats = { matched: number, extras: number, missing: number }

--- A live instance main never had: Studio's own migration additions land here.
--- It gets a `new:` id so events about it still resolve, and the whole subtree
--- under it goes the same way — but it is NEVER journalled as an `add`, it
--- simply exists on this side.
local function markExtra(instance: Instance, service: Instance, stats: AlignStats)
	newCounter += 1
	assign(instance, `new:{newCounter}`)
	stats.extras += 1

	for _, child in liveChildrenOf(instance, service) do
		markExtra(child, service, stats)
	end
end

--- Align one parent's children against the shape's, in order, allowing skips
--- on either side. A greedy two-pointer with a bounded lookahead: whichever
--- side has the nearer match is the side that skips, so a run of insertions on
--- one side does not desynchronise everything after it.
local function align(
	liveList: { Instance },
	shapeList: { ShapeNode },
	base: number,
	service: Instance,
	stats: AlignStats
)
	local i, j = 1, 1

	while i <= #liveList and j <= #shapeList do
		local live = liveList[i]
		local node = shapeList[j]

		if live.Name == node.name and live.ClassName == node.class then
			assign(live, tostring(base + node.index))
			stats.matched += 1

			align(liveChildrenOf(live, service), node.children, base, service, stats)

			i += 1
			j += 1
			continue
		end

		local aheadShape: number? = nil

		for k = j + 1, math.min(#shapeList, j + Journal.ALIGN_WINDOW) do
			if shapeList[k].name == live.Name and shapeList[k].class == live.ClassName then
				aheadShape = k
				break
			end
		end

		local aheadLive: number? = nil

		for k = i + 1, math.min(#liveList, i + Journal.ALIGN_WINDOW) do
			if liveList[k].Name == node.name and liveList[k].ClassName == node.class then
				aheadLive = k
				break
			end
		end

		if aheadShape ~= nil and (aheadLive == nil or (aheadShape - j) <= (aheadLive - i)) then
			for k = j, (aheadShape :: number) - 1 do
				stats.missing += shapeList[k].size
			end

			j = aheadShape :: number
		elseif aheadLive ~= nil then
			for k = i, (aheadLive :: number) - 1 do
				markExtra(liveList[k], service, stats)
			end

			i = aheadLive :: number
		else
			-- Neither side can find the other within the window: charge one to
			-- each and move both on, so the walk always makes progress.
			markExtra(live, service, stats)
			stats.missing += node.size

			i += 1
			j += 1
		end
	end

	while i <= #liveList do
		markExtra(liveList[i], service, stats)
		i += 1
	end

	while j <= #shapeList do
		stats.missing += shapeList[j].size
		j += 1
	end
end

--- Number the live tree by aligning it against main's shape rather than by
--- counting. Returns `{ok, matched, extras, missing, count, shapeCount,
--- services}`; on a refusal every id is dropped again, so a refused adopt
--- leaves no half-numbering behind.
function Journal.adoptShape(shape: Shape, services: { Instance }?): { [string]: any }
	local list = services or Journal.services()

	table.clear(idOf)
	table.clear(byId)
	table.clear(addedRoots)

	newCounter = 0

	local byService: { [string]: ShapeService } = {}

	for _, entry in shape.services do
		byService[entry.service] = entry
	end

	local stats: AlignStats = { matched = 0, extras = 0, missing = 0 }
	local report: { { [string]: any } } = {}
	local starved: { string } = {}

	for _, service in list do
		byId[serviceIdOf(service)] = service

		local entry = byService[service.Name]
		local before = stats.matched

		if entry == nil then
			-- Main had no shape for this service at all: everything live in it
			-- is an extra.
			for _, child in liveChildrenOf(service, service) do
				markExtra(child, service, stats)
			end
		else
			align(liveChildrenOf(service, service), parseShapeNodes(entry.nodes), entry.base, service, stats)

			if #entry.nodes > 0 and stats.matched == before then
				table.insert(starved, service.Name)
			end
		end

		table.insert(report, {
			service = service.Name,
			expected = if entry ~= nil then #entry.nodes else 0,
			matched = stats.matched - before,
		})
	end

	local expected = shape.count
	local ratio = if expected > 0 then stats.matched / expected else 1

	local ok = #starved == 0 and ratio >= Journal.ALIGN_MIN_RATIO

	local result: { [string]: any } = {
		ok = ok,
		matched = stats.matched,
		extras = stats.extras,
		missing = stats.missing,
		count = stats.matched + stats.extras,
		shapeCount = expected,
		ratio = ratio,
		services = report,
	}

	if not ok then
		table.clear(idOf)
		table.clear(byId)

		newCounter = 0
		result.unmatchedServices = starved

		return result
	end

	-- The id space both Studios share is main's numbering, not the live count.
	checkpointCount = expected

	return result
end

--------------------------------------------------------------------------------
-- Property codec (the daemon's encoding — Lib/Dom, as Core/Processor/Read uses)
--------------------------------------------------------------------------------

--- Encode one live property the way the read processor encodes it for the
--- daemon: the canonical descriptor's type from the reflection database, then
--- `Dom.EncodedValue`. nil when the property cannot be read or encoded.
function Journal.encodeProperty(instance: Instance, prop: string): any?
	local defaults = Dom.getDefaultProperties(instance.ClassName)
	local default = defaults[prop]

	if default == nil then
		return nil
	end

	local readOk, value = Dom.readProperty(instance, prop)

	if not readOk then
		return nil
	end

	local encodeOk, encoded = Dom.EncodedValue.encode(value, (next(default)))

	return if encodeOk then encoded else nil
end

--- Encode an attribute value (attributes carry no reflection descriptor, so
--- the naive encoder picks the type from the Luau value).
local function encodeAttribute(value: any): any?
	if value == nil then
		return nil
	end

	local ok, encoded = Dom.EncodedValue.encodeNaive(value)

	return if ok then encoded else nil
end

local function decodeValue(encoded: any): (boolean, any)
	if encoded == nil then
		return true, nil
	end

	return Dom.EncodedValue.decode(encoded)
end

--- Every non-default, encodable property of an instance, in wire form. Used
--- by the fingerprint sweep to catch sets no signal reported.
local function snapshotProperties(instance: Instance): { [string]: any }
	local properties: { [string]: any } = {}

	for prop in Dom.getDefaultProperties(instance.ClassName) do
		if SKIP_PROPERTIES[prop] then
			continue
		end

		if prop == "Source" and instance:IsA("LuaSourceContainer") then
			continue
		end

		local encoded = Journal.encodeProperty(instance, prop)

		if encoded ~= nil then
			properties[prop] = encoded
		end
	end

	return properties
end

--- Structural equality over encoded values (tables of plain JSON scalars).
local function sameValue(a: any, b: any): boolean
	if a == b then
		return true
	end

	if type(a) ~= "table" or type(b) ~= "table" then
		return false
	end

	for key, value in a do
		if not sameValue(value, b[key]) then
			return false
		end
	end

	for key in b do
		if a[key] == nil then
			return false
		end
	end

	return true
end

--------------------------------------------------------------------------------
-- Emitting and coalescing
--------------------------------------------------------------------------------

local function nowMillis(): number
	local ok, millis = pcall(function()
		return DateTime.now().UnixTimestampMillis
	end)

	return if ok then millis else math.floor(os.time() * 1000)
end

local function coalesceKey(id: string, op: string, event: Event): string
	local discriminator = event.prop or event.name or ""

	if op == "rename" or op == "add" or op == "move" or op == "remove" then
		discriminator = ""
	end

	return `{id}\0{op}\0{discriminator}`
end

local function forget(id: string, key: string)
	if pending[key] ~= nil then
		pending[key] = nil
		pendingCount -= 1
	end

	local keys = pendingKeys[id]

	if keys ~= nil then
		keys[key] = nil
	end
end

local function purge(id: string): boolean
	local keys = pendingKeys[id]

	if keys == nil then
		return false
	end

	local hadAdd = false

	for key in keys do
		local event = pending[key]

		if event ~= nil and event.op == "add" then
			hadAdd = true
		end

		if pending[key] ~= nil then
			pending[key] = nil
			pendingCount -= 1
		end
	end

	pendingKeys[id] = nil

	return hadAdd
end

--- Append one event. Coalescing happens here, not at flush time, so the
--- pending set stays bounded by the number of distinct `(id, op, prop)` an
--- editor touched between two flushes.
local function emit(id: string, op: string, fields: Event?)
	if not active or muted then
		return
	end

	seq += 1

	local event: Event = { t = nowMillis(), seq = seq, id = id, op = op }

	if fields ~= nil then
		for key, value in fields do
			event[key] = value
		end
	end

	if recording ~= nil then
		event.rec = recording
	end

	table.insert(history, event)

	if op == "remove" then
		-- Added then removed before a flush: nothing about this id survives.
		if purge(id) then
			addedRoots[id] = nil
			return
		end

		addedRoots[id] = nil
	end

	local key = coalesceKey(id, op, event)

	if pending[key] == nil then
		pendingCount += 1

		local keys = pendingKeys[id]

		if keys == nil then
			keys = {}
			pendingKeys[id] = keys
		end

		keys[key] = true
	end

	pending[key] = event
end

Journal.__emit = emit

--------------------------------------------------------------------------------
-- Blobs
--------------------------------------------------------------------------------

--- Pin already-serialized bytes and return the id that names them. Same shape
--- as the clipboard's pin (Remote/Transfers), drained by `journal_blob_read`.
function Journal.captureBlobBytes(bytes: buffer): string?
	local blobId = Hex.encode(generateRef())

	if Transfers.putOut(blobs, blobId, bytes, BufferHash.hex(bytes), nil, os.clock()) ~= "ok" then
		return nil
	end

	return blobId
end

--- Serialize a subtree and pin it, returning the blob id the `add` carries.
function Journal.captureBlob(instance: Instance): string?
	local service = serializationService()

	if service == nil then
		return nil
	end

	local ok, bytes = pcall(function()
		return service:SerializeInstancesAsync({ instance })
	end)

	if not ok or typeof(bytes) ~= "buffer" then
		return nil
	end

	return Journal.captureBlobBytes(bytes :: buffer)
end

function Journal.readBlob(blobId: string, offset: number, maxLen: number)
	return Transfers.readOut(blobs, blobId, offset, maxLen, os.clock())
end

--- Size and digest of a pinned blob, so a caller can size its pull (and skip
--- the `open` round trip when the checkpoint already handed it both).
--- Refreshes the entry's sliding TTL.
function Journal.blobInfo(blobId: string): { bytes: number, sha256: string }?
	local entry = Transfers.getOut(blobs, blobId, os.clock())

	if entry == nil then
		return nil
	end

	return { bytes = buffer.len(entry.bytes), sha256 = entry.sha256 }
end

--- Pull a blob back OUT of this Studio's own daemon: `GET /journal/blob/<id>`
--- answers raw bytes, which the caller decodes. The clone's shape alignment is
--- the one place the plugin reads from the daemon instead of the daemon
--- reading from the plugin — the shape was pinned in MAIN's Studio, so this
--- Studio has no copy of it to serve.
---
--- Returns `(body, nil)` or `(nil, reason)`.
function Journal.fetchBlob(blobId: string): (string?, string?)
	if endpoint == nil then
		return nil, "the journal has no daemon to fetch from — this Studio is not connected"
	end

	local scheme = if (endpoint :: any).https then "https" else "http"
	local url = `{scheme}://{(endpoint :: any).host}:{(endpoint :: any).port}/journal/blob/{blobId}`

	local ok, body = pcall(function()
		return Http.getJson(url):expect().Body
	end)

	if not ok then
		return nil, `GET /journal/blob/{blobId} failed: {tostring(body)}`
	end

	if type(body) ~= "string" or body == "" then
		return nil, `GET /journal/blob/{blobId} answered no bytes`
	end

	return body :: string, nil
end

--- Drop a pinned blob. Returns false when there was nothing to drop, so a
--- double close reads as "already gone" rather than as an error.
function Journal.dropBlob(blobId: string): boolean
	if Transfers.getOut(blobs, blobId, os.clock()) == nil then
		return false
	end

	Transfers.dropOut(blobs, blobId)

	return true
end

--- Re-capture every added root still in the tree whose blob has gone stale,
--- re-emitting its `add` with the fresh blob (coalescing keeps the latest).
local function refreshBlobs()
	local now = os.clock()

	for id, root in addedRoots do
		if not root.instance:IsDescendantOf(game) then
			addedRoots[id] = nil
			continue
		end

		if now - root.capturedAt < Journal.BLOB_REFRESH_SECONDS then
			continue
		end

		local blobId = Journal.captureBlob(root.instance)

		root.capturedAt = os.clock()

		if blobId ~= nil then
			emit(id, "add", {
				parent = root.parent,
				name = root.instance.Name,
				class = root.class,
				blob = blobId,
			})
		end
	end
end

--------------------------------------------------------------------------------
-- Transport
--------------------------------------------------------------------------------

--- Where `POST /journal/append` goes. Core stamps it once the transport has
--- resolved, so the journal always follows the daemon discovery actually
--- connected to rather than the Settings values.
function Journal.setEndpoint(host: string?, port: (string | number)?, https: boolean?)
	if host == nil or port == nil then
		endpoint = nil
		return
	end

	endpoint = { host = host, port = tostring(port), https = https == true }
end

local function drain(): { Event }
	local batch: { Event } = {}

	for _, event in pending do
		table.insert(batch, event)
	end

	table.sort(batch, function(a, b)
		return a.seq < b.seq
	end)

	table.clear(pending)
	table.clear(pendingKeys)
	pendingCount = 0

	return batch
end

local function requeue(batch: { Event })
	for _, event in batch do
		local key = coalesceKey(event.id, event.op, event)

		if pending[key] == nil then
			pendingCount += 1

			local keys = pendingKeys[event.id]

			if keys == nil then
				keys = {}
				pendingKeys[event.id] = keys
			end

			keys[key] = true
			pending[key] = event
		end
	end
end

--- Push everything pending. Returns `{flushed, pending}`. A failed POST puts
--- the batch back rather than losing it — the daemon appends idempotently by
--- `seq`, so a duplicate batch is cheaper than a hole.
function Journal.flush(): { flushed: number, pending: number }
	if not active or checkpointId == nil then
		return { flushed = 0, pending = pendingCount }
	end

	refreshBlobs()

	local batch = drain()

	if #batch == 0 then
		return { flushed = 0, pending = 0 }
	end

	if endpoint == nil then
		requeue(batch)
		return { flushed = 0, pending = pendingCount }
	end

	local scheme = if endpoint.https then "https" else "http"
	local url = `{scheme}://{endpoint.host}:{endpoint.port}/journal/append`

	local ok = pcall(function()
		Http.postJson(url, { checkpoint = checkpointId, events = batch }):expect()
	end)

	if not ok then
		requeue(batch)
		Log.debug(`Journal: /journal/append failed, {#batch} events requeued`)

		return { flushed = 0, pending = pendingCount }
	end

	flushedTotal += #batch
	lastFlushAt = os.clock()

	return { flushed = #batch, pending = pendingCount }
end

--------------------------------------------------------------------------------
-- Listeners
--------------------------------------------------------------------------------

--- The id an event should name as this instance's parent: the parent's own
--- id, or `svc:<Name>` when the parent is one of the eleven content services
--- (they are not numbered, but a top-level add still needs a parent to name).
local function parentIdOf(instance: Instance): string?
	local parent = instance.Parent

	if parent == nil then
		return nil
	end

	local id = idOf[parent]

	if id ~= nil then
		return id
	end

	local serviceId = serviceIdOf(parent)

	return if byId[serviceId] == parent then serviceId else nil
end

local function onChanged(instance: Instance, prop: string)
	local id = idOf[instance]

	if id == nil then
		return
	end

	if SKIP_PROPERTIES[prop] then
		return
	end

	if prop == "Name" then
		emit(id, "rename", { name = instance.Name })
		return
	end

	-- git owns script bodies; the replay never touches them either.
	if prop == "Source" and instance:IsA("LuaSourceContainer") then
		return
	end

	local encoded = Journal.encodeProperty(instance, prop)

	if encoded ~= nil then
		emit(id, "set", { prop = prop, value = encoded })

		local snapshot = snapshots[instance]

		if snapshot ~= nil then
			snapshot[prop] = encoded
		end
	end
end

--- Reconcile an instance against its last snapshot, emitting the sets and the
--- rename that no signal reported. Returns how many events it emitted.
local function reconcile(instance: Instance): number
	local id = idOf[instance]

	if id == nil or not instance:IsDescendantOf(game) then
		return 0
	end

	local previous = snapshots[instance]
	local current = snapshotProperties(instance)

	snapshots[instance] = current

	if previous == nil then
		return 0
	end

	local emitted = 0

	for prop, value in current do
		if not sameValue(previous[prop], value) then
			if prop == "Name" then
				emit(id, "rename", { name = instance.Name })
			else
				emit(id, "set", { prop = prop, value = value })
			end

			emitted += 1
		end
	end

	return emitted
end

--- Connect the lazy per-instance signals. Called for anything the user
--- selected in Studio and for anything a plugin op wrote through.
---
--- `Changed` on a `ValueBase` reports the new *value*, not a property name
--- (a long-standing engine quirk), so those get explicit property signals.
local function watch(instance: Instance)
	if watched[instance] or idOf[instance] == nil then
		return
	end

	watched[instance] = true

	if instance:IsA("ValueBase") then
		for _, prop in { "Name", "Value" } do
			local ok, signal = pcall(function()
				return instance:GetPropertyChangedSignal(prop)
			end)

			if ok then
				table.insert(
					connections,
					signal:Connect(function()
						onChanged(instance, prop)
					end)
				)
			end
		end
	else
		table.insert(
			connections,
			instance.Changed:Connect(function(prop: string)
				onChanged(instance, prop)
			end)
		)
	end

	table.insert(
		connections,
		instance.AttributeChanged:Connect(function(name: string)
			local id = idOf[instance]

			if id ~= nil then
				emit(id, "attr", { name = name, value = encodeAttribute(instance:GetAttribute(name)) })
			end
		end)
	)
end

--- The write-handler hook (Handlers/Write calls this after every `set`,
--- `attr`, `tag`, `rename` and `move`). Connecting here is what makes the
--- *next* edit of that instance observable; the op's own write is reconciled
--- against the stored snapshot, so only the very first touch of an instance
--- nobody has selected relies on the daemon already knowing what it asked for.
function Journal.touched(instance: Instance?, rec: string?)
	if not active or instance == nil or idOf[instance] == nil then
		return
	end

	local previous = recording

	if rec ~= nil then
		recording = rec
	end

	watch(instance)
	recent[instance] = os.clock()
	reconcile(instance)

	recording = previous
end

--- The recording name stamped on every event until it is cleared (`rec`).
function Journal.setRecording(name: string?)
	recording = name
end

local function subscribeTags()
	local ok, tags = pcall(function()
		return CollectionService:GetAllTags()
	end)

	if not ok or type(tags) ~= "table" then
		return
	end

	for _, tag in tags do
		if subscribedTags[tag] then
			continue
		end

		subscribedTags[tag] = true

		local okSignals = pcall(function()
			table.insert(
				connections,
				CollectionService:GetInstanceAddedSignal(tag):Connect(function(instance: Instance)
					local id = idOf[instance]

					if id ~= nil then
						emit(id, "tag", { name = tag, on = true })
					end
				end)
			)

			table.insert(
				connections,
				CollectionService:GetInstanceRemovedSignal(tag):Connect(function(instance: Instance)
					local id = idOf[instance]

					if id ~= nil then
						emit(id, "tag", { name = tag, on = false })
					end
				end)
			)
		end)

		if not okSignals then
			subscribedTags[tag] = nil
		end
	end
end

local function onDescendantAdded(instance: Instance, service: Instance)
	if muted or Journal.isExcluded(instance, service) or idOf[instance] ~= nil then
		return
	end

	local parent = parentIdOf(instance)

	if parent == nil then
		return
	end

	newCounter += 1

	local id = `new:{newCounter}`
	assign(instance, id)

	local class = instance.ClassName

	-- An *added root* is one whose parent already had an id before this add:
	-- everything under it travels inside its blob, so only the root is
	-- serialized and re-serialized.
	local isRoot = string.sub(parent, 1, 4) ~= "new:"

	local blobId: string? = nil

	if isRoot then
		blobId = Journal.captureBlob(instance)

		addedRoots[id] = {
			instance = instance,
			parent = parent,
			name = instance.Name,
			class = class,
			capturedAt = os.clock(),
		}
	end

	emit(id, "add", { parent = parent, name = instance.Name, class = class, blob = blobId })
end

local function onDescendantRemoving(instance: Instance)
	local id = idOf[instance]

	if muted or id == nil then
		return
	end

	-- The signal fires *before* the removal, so the old parent is still here.
	local oldParent = instance.Parent

	task.defer(function()
		if instance:IsDescendantOf(game) then
			local parent = instance.Parent

			if parent ~= oldParent then
				local parentId = parentIdOf(instance)

				if parentId ~= nil then
					emit(id, "move", { parent = parentId })
				end
			end

			return
		end

		emit(id, "remove", {})
	end)
end

local function sweep()
	local now = os.clock()
	local looked = 0

	for instance, at in recent do
		if now - at > Journal.SELECTION_WINDOW_SECONDS then
			recent[instance] = nil
			continue
		end

		if looked >= Journal.SWEEP_MAX_INSTANCES then
			break
		end

		looked += 1
		reconcile(instance)
	end
end

local function startLoop()
	if looping then
		return
	end

	looping = true

	task.spawn(function()
		local lastFlush = os.clock()
		local lastSweep = os.clock()

		while active do
			task.wait(1)

			if not active then
				break
			end

			local now = os.clock()

			if now - lastFlush >= Journal.FLUSH_SECONDS and (pendingCount > 0 or next(addedRoots) ~= nil) then
				lastFlush = now
				subscribeTags()

				pcall(Journal.flush)
			end

			if now - lastSweep >= Journal.SWEEP_SECONDS then
				lastSweep = now
				pcall(sweep)
			end
		end

		looping = false
	end)
end

local function disconnectAll()
	for _, connection in connections do
		pcall(function()
			connection:Disconnect()
		end)
	end

	table.clear(connections)
	table.clear(watched)
	table.clear(subscribedTags)
end

--- Start recording against a checkpoint. The tree must already be numbered
--- (`Journal.checkpoint`). Idempotent per checkpoint.
function Journal.start(checkpoint: string, services: { Instance }?)
	if active and checkpointId == checkpoint then
		return
	end

	disconnectAll()

	active = true
	checkpointId = checkpoint

	local list = services or Journal.services()

	for _, service in list do
		table.insert(
			connections,
			service.DescendantAdded:Connect(function(instance: Instance)
				onDescendantAdded(instance, service)
			end)
		)

		table.insert(connections, service.DescendantRemoving:Connect(onDescendantRemoving))
	end

	local okSelection, selection = pcall(function()
		return game:GetService("Selection")
	end)

	if okSelection then
		table.insert(
			connections,
			(selection :: any).SelectionChanged:Connect(function()
				local okGet, selected = pcall(function()
					return (selection :: any):Get()
				end)

				if not okGet then
					return
				end

				for _, instance in selected do
					watch(instance)
					recent[instance] = os.clock()

					if snapshots[instance] == nil and idOf[instance] ~= nil then
						snapshots[instance] = snapshotProperties(instance)
					end
				end
			end)
		)
	end

	subscribeTags()
	startLoop()

	Log.stage(`Journal: recording checkpoint {checkpoint} over {checkpointCount} instances`)
end

function Journal.stop()
	active = false
	checkpointId = nil
	disconnectAll()
end

--- Main's side: remember where this checkpoint sits in main's own stream, so
--- `Journal.replay` knows which of main's events are "since the checkpoint".
function Journal.mark(checkpoint: string)
	marks[checkpoint] = #history
end

function Journal.status(): { [string]: any }
	return {
		active = active,
		branch = Branch.isBranch(),
		checkpoint = checkpointId,
		count = checkpointCount,
		pending = pendingCount,
		flushed = flushedTotal,
		seq = seq,
		lastFlushAt = lastFlushAt,
		connected = endpoint ~= nil,
	}
end

function Journal.idOf(instance: Instance): string?
	return idOf[instance]
end

function Journal.instanceOf(id: string): Instance?
	return byId[id]
end

function Journal.count(): number
	return checkpointCount
end

--- The checkpoint this Studio is recording against, or nil when the recorder
--- was never started. `branch up` has to name it to replay a clone's own
--- journal back into it.
function Journal.currentCheckpoint(): string?
	return checkpointId
end

--------------------------------------------------------------------------------
-- Replay (main's side of a merge)
--------------------------------------------------------------------------------

type Touch = {
	any: boolean,
	removed: boolean,
	movedTo: string?,
}

--- Everything main's own stream did to each id after the branch's checkpoint
--- mark. An unknown checkpoint means main recorded nothing for it, which is
--- the same as "main touched nothing": every branch event applies.
local function touchesSince(checkpoint: string): { [string]: Touch }
	local touches: { [string]: Touch } = {}
	local mark = marks[checkpoint]

	if mark == nil then
		return touches
	end

	for index = mark + 1, #history do
		local event = history[index]
		local touch = touches[event.id]

		if touch == nil then
			touch = { any = false, removed = false, movedTo = nil }
			touches[event.id] = touch
		end

		touch.any = true

		if event.op == "remove" then
			touch.removed = true
		elseif event.op == "move" then
			touch.movedTo = event.parent
		end
	end

	return touches
end

--- Would applying this branch event change anything? "An id both touched is a
--- conflict unless the resulting values are equal" — the cheapest and most
--- exact reading of "equal" is: main is already in the state the branch event
--- asks for.
local function alreadyEqual(event: Event, instance: Instance?): boolean
	if instance == nil then
		return false
	end

	local op = event.op

	if op == "rename" then
		return instance.Name == event.name
	elseif op == "move" then
		local parent = instance.Parent

		return parent ~= nil and (idOf[parent] == event.parent or serviceIdOf(parent) == event.parent)
	elseif op == "set" then
		return sameValue(Journal.encodeProperty(instance, event.prop), event.value)
	elseif op == "attr" then
		return sameValue(encodeAttribute(instance:GetAttribute(event.name)), event.value)
	elseif op == "tag" then
		local ok, has = pcall(function()
			return CollectionService:HasTag(instance, event.name)
		end)

		return ok and has == (event.on == true)
	end

	return false
end

--- What main currently holds for the dimension this event names — the `ours`
--- side of a conflict record.
local function oursValue(event: Event, instance: Instance?): any
	if instance == nil then
		return nil
	end

	local op = event.op

	if op == "rename" then
		return instance.Name
	elseif op == "move" then
		local parent = instance.Parent

		return if parent == nil then nil else (idOf[parent] or serviceIdOf(parent))
	elseif op == "set" then
		return Journal.encodeProperty(instance, event.prop)
	elseif op == "attr" then
		return encodeAttribute(instance:GetAttribute(event.name))
	elseif op == "tag" then
		local ok, has = pcall(function()
			return CollectionService:HasTag(instance, event.name)
		end)

		return ok and has == true
	end

	return nil
end

local function theirsValue(event: Event): any
	local op = event.op

	if op == "rename" then
		return event.name
	elseif op == "move" or op == "add" then
		return event.parent
	elseif op == "set" or op == "attr" then
		return event.value
	elseif op == "tag" then
		return event.on == true
	end

	return nil
end

local function pathOf(instance: Instance?): string?
	if instance == nil then
		return nil
	end

	local ok, full = pcall(function()
		return instance:GetFullName()
	end)

	return if ok then full else nil
end

local function tryBeginRecording(name: string): string?
	local id: string? = nil

	pcall(function()
		local result = (ChangeHistoryService :: any):TryBeginRecording(name, name)

		if type(result) == "string" then
			id = result
		end
	end)

	return id
end

local function finishRecording(id: string)
	pcall(function()
		(ChangeHistoryService :: any):FinishRecording(id, Enum.FinishRecordingOperation.Commit)
	end)
end

--- Deserialize one blob (`{[blobId] = <base64 rbxm>}` as the daemon ships it)
--- and return its first root, detached.
local function fromBlob(encoded: any): Instance?
	if type(encoded) ~= "string" then
		return nil
	end

	local service = serializationService()

	if service == nil then
		return nil
	end

	local raw = Base64.decode(encoded)

	if raw == nil then
		return nil
	end

	local ok, roots = pcall(function()
		return service:DeserializeInstancesAsync(buffer.fromstring(raw :: string))
	end)

	if not ok or type(roots) ~= "table" or #roots == 0 then
		return nil
	end

	for index = 2, #roots do
		pcall(function()
			roots[index]:Destroy()
		end)
	end

	return roots[1]
end

local function applyEvent(
	event: Event,
	instance: Instance?,
	blobData: { [string]: any },
	created: { [string]: Instance }
): boolean
	local op = event.op

	if op == "add" then
		-- A blob refresh re-emits an `add` for a root already in the stream
		-- (see refreshBlobs), so a second one is not a second instance.
		if created[event.id] ~= nil then
			return true
		end

		local parent = created[event.parent] or byId[event.parent]

		if parent == nil then
			return false
		end

		local subject = fromBlob(blobData[event.blob])

		if subject == nil then
			local ok, fresh = pcall(function()
				return Instance.new(event.class)
			end)

			if not ok then
				return false
			end

			subject = fresh
		end

		local added: Instance = subject

		local ok = pcall(function()
			added.Name = event.name
			added.Parent = parent
		end)

		if not ok then
			return false
		end

		created[event.id] = added

		-- Registered under the id the journal gave it, so every later event in
		-- the same stream resolves. The session counter is pushed past it so a
		-- live add after the replay cannot reuse the number.
		assign(added, event.id)

		local replayed = tonumber(string.match(event.id, "^new:(%d+)$"))

		if replayed ~= nil and replayed > newCounter then
			newCounter = replayed
		end

		return true
	end

	if instance == nil then
		return false
	end

	if op == "remove" then
		return (pcall(function()
			instance:Destroy()
		end))
	elseif op == "move" then
		local parent = created[event.parent] or byId[event.parent]

		if parent == nil then
			return false
		end

		return (pcall(function()
			instance.Parent = parent
		end))
	elseif op == "rename" then
		return (pcall(function()
			instance.Name = event.name
		end))
	elseif op == "set" then
		-- Never `Source`: wsync writes every script to disk as it changes, so
		-- git owns those bytes and the replay must not fight the worktree.
		if event.prop == "Source" and instance:IsA("LuaSourceContainer") then
			return false
		end

		local decodeOk, value = decodeValue(event.value)

		if not decodeOk then
			return false
		end

		return (Dom.writeProperty(instance, event.prop, value))
	elseif op == "attr" then
		local decodeOk, value = decodeValue(event.value)

		if not decodeOk then
			return false
		end

		return (pcall(function()
			instance:SetAttribute(event.name, value)
		end))
	elseif op == "tag" then
		return (
			pcall(function()
				if event.on == true then
					CollectionService:AddTag(instance, event.name)
				else
					CollectionService:RemoveTag(instance, event.name)
				end
			end)
		)
	end

	return false
end

export type Conflict = {
	n: number,
	id: string,
	path: string?,
	kind: string,
	ours: any,
	theirs: any,
}

--- Replay a branch's stream against main's live tree inside one
--- ChangeHistory recording (one Studio undo). Three conflict rules, exactly
--- as docs/branches.md states them:
---
---   * an id touched in main's own stream since the checkpoint AND in the
---     branch's events — conflict, unless main already holds the value;
---   * a remove on one side and any touch on the other — conflict;
---   * a move on both sides to different parents — conflict.
---
--- Conflicts are skipped and returned. A second call carrying
--- `decisions = {[n] = "ours" | "theirs"}` applies the chosen side.
---
--- `own` is the other mode: `branch up` rebuilding a branch clone from the
--- branch's OWN journal. There is no second journal to conflict with, so
--- everything applies, and the recorder is muted for the duration — the
--- changes being replayed already ARE the journal, and recording them again
--- would push every one of them a second time on the next flush. The caller
--- (Handlers/Branch) is what checks that `checkpoint` is this clone's own.
function Journal.replay(
	checkpoint: string,
	events: { Event },
	blobData: { [string]: any }?,
	decisions: { [any]: string }?,
	own: boolean?
): { [string]: any }
	local reconstructing = own == true
	local touches = if reconstructing then {} :: { [string]: Touch } else touchesSince(checkpoint)
	local blobsIn = blobData or {}
	local chosen = decisions or {}
	local created: { [string]: Instance } = {}

	local conflicts: { Conflict } = {}
	local applied = 0
	local skipped = 0

	local label = if reconstructing then "branch restore" else "branch replay"

	local recordingId = tryBeginRecording(`WSync: {label} {checkpoint}`)

	local wasMuted = muted
	muted = reconstructing

	Journal.setRecording(`{label} {checkpoint}`)

	local ok, caught = pcall(function()
		for n, event in events do
			if type(event) ~= "table" or type(event.id) ~= "string" or type(event.op) ~= "string" then
				skipped += 1
				continue
			end

			local instance = created[event.id] or byId[event.id]
			local touch = touches[event.id]
			local decision = chosen[tostring(n)] or chosen[n]

			local kind: string? = nil

			if touch ~= nil then
				if touch.removed or event.op == "remove" then
					kind = "remove"
				elseif event.op == "move" and touch.movedTo ~= nil and touch.movedTo ~= event.parent then
					kind = "move"
				elseif not alreadyEqual(event, instance) then
					kind = "touch"
				end
			end

			if kind ~= nil and decision == nil then
				table.insert(conflicts, {
					n = n,
					id = event.id,
					path = pathOf(instance),
					kind = kind,
					ours = oursValue(event, instance),
					theirs = theirsValue(event),
				})

				skipped += 1
				continue
			end

			if decision == "ours" and not reconstructing then
				skipped += 1
				continue
			end

			if applyEvent(event, instance, blobsIn, created) then
				applied += 1
			else
				skipped += 1
			end
		end
	end)

	Journal.setRecording(nil)

	muted = wasMuted

	if recordingId ~= nil then
		finishRecording(recordingId)
	end

	if not ok then
		Log.warn(`Journal replay {checkpoint} stopped early: {tostring(caught)}`)
	end

	return {
		applied = applied,
		skipped = skipped,
		conflicts = conflicts,
		own = reconstructing,
		stopped = if ok then nil else tostring(caught),
	}
end

return Journal
