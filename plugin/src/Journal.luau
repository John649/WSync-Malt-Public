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
-- Both Studios hold identical trees at the FIRST checkpoint, so both produce
-- identical numbering. Instances created afterwards get `"new:<m>"`, m a
-- per-session counter — `"new:<tag>:<m>"` on a branch clone, where `tag` is
-- the branch name (`Journal.setOrigin`), so two clones cut from the same main
-- cannot mint the same id for two different instances and a merge cannot
-- mistake one branch's add for another's. The eleven services themselves are
-- not numbered (they exist in every place); they are addressable as
-- `"svc:<Name>"` so an event about a top-level instance has a parent to name.
-- `id -> Instance` and `Instance -> id` are held strongly for the Studio's
-- lifetime, exactly as the contract requires: an id must keep meaning
-- something after the instance leaves the tree.
--
-- STABLE IDS ACROSS CHECKPOINTS (two or more live branches). A second
-- checkpoint — `wsync branch new B` while branch A is still live — does NOT
-- renumber main. Numbering is incremental: every instance that already has an
-- id keeps it, and only instances without one are numbered, continuing the
-- counter so an id is never reused. Renumbering silently repointed every id in
-- A's journal at a different instance, which is the one thing this design
-- cannot survive. It follows that the checkpoint shape cannot be positional
-- any more: SHAPE_VERSION 2 carries each node's id explicitly. v1's
-- `base + pre-order index` is still read, so a clone cut by an older plugin
-- adopts exactly as it did before.
--
-- ONE STREAM, MANY MARKS. Main records ONE event stream for the whole
-- session, named by the first checkpoint it took, and a mark per branch
-- (`Journal.mark`) fixes where in that stream the branch was cut.
-- `Journal.start` on an already-recording Studio therefore adds the mark and
-- leaves the stream alone — restarting it cut every older branch off from
-- main's later edits, which is what its conflict detection is made of. The
-- daemon fans one `/journal/append` out to every marked checkpoint's file
-- (src/server/journal.rs), so each live branch's `<checkpoint>.jsonl` stays a
-- complete record of main since its own mark.
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
-- How many instances the initial observation pass connects before it yields.
-- Connecting two signals on every instance of a 157k-instance place in one
-- synchronous go would blow the remote op's timeout and freeze Studio's main
-- thread; the pass runs detached and `journal_status` reports its progress.
Journal.OBSERVE_CHUNK = 2000
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

-- The parent id we last recorded for each instance. This is the ONLY thing a
-- move is decided against, because `instance.Parent` cannot be trusted to still
-- hold the old parent while `DescendantRemoving` runs: the engine may have
-- already completed the assignment, in which case "old" and "new" compare equal
-- and a reparent within one service vanishes without a trace. It also survives
-- the instance leaving the tree, which is what lets a mass removal work out
-- which of its ids were descendants of another.
local parentOf: { [Instance]: string } = {}
local newCounter = 0

-- The high-water mark of the plain numeric id space (`"<n>"`). A checkpoint
-- numbers only what has no id yet and continues from here, so a second
-- checkpoint taken while a branch is live adds ids without disturbing one.
local numberCounter = 0

-- What this Studio stamps into the ids it mints itself: nil on main
-- (`new:<m>`), the branch name on a clone (`new:<name>:<m>`). Two clones cut
-- from the same main number their migration extras identically, so without
-- this their first added instances collide on one id in main at merge time.
local idTag: string? = nil

local seq = 0

-- Bumped by every emit. A pending add is only re-captured when this moved
-- since its last capture: an idle branch used to re-serialize every added
-- subtree every BLOB_REFRESH_SECONDS forever (363 adds for one folder in
-- the first DungeonDemo branch), each with a fresh blob pinned in memory.
local emitTick = 0
local pending: { [string]: Event } = {}
local pendingKeys: { [string]: { [string]: boolean } } = {}
local pendingCount = 0

-- Every event ever emitted, uncoalesced: main's side of a merge compares a
-- branch's stream against the slice of this one that follows the branch's
-- checkpoint mark.
local history: { Event } = {}
local marks: { [string]: number } = {}

-- What a replay has already done, per checkpoint, ACROSS CALLS. `branch
-- finish` is not a single shot: a run that reports conflicts is followed by
-- `branch pick` and another run carrying the decisions, and the second run
-- ships the same journal. Without a ledger it re-applies every event that did
-- not conflict the first time, which is how main ended up with a second
-- GammaPart. Parked conflicts are deliberately NOT recorded here, so they are
-- the only thing a rerun reconsiders.
local replayApplied: { [string]: { [number]: boolean } } = {}
local replayCreated: { [string]: { [string]: Instance } } = {}

local connections: { RBXScriptConnection } = {}
local subscribedTags: { [string]: boolean } = {}
local watchedServices: { [Instance]: boolean } = {}

-- Per-instance observation. `instanceConnections` is keyed by instance so the
-- connections of one that leaves the tree can be dropped without touching
-- anything else — its id stays, because events already in flight still name
-- it. `dirty` holds what changed since the last drain: property names, and
-- attribute names prefixed with `@` (no Roblox property starts with one).
local instanceConnections: { [Instance]: { RBXScriptConnection } } = {}
local dirty: { [Instance]: { [string]: boolean } } = {}
local dirtyRec: { [Instance]: string } = {}
local removalBatch: { [Instance]: boolean } = {}
local removalScheduled = false
local drainScheduled = false
local observed = 0
local observing = false

-- Instances whose removal has already been recorded. A destroy reaches the
-- recorder down two independent paths -- the service's `DescendantRemoving`
-- and the instance's own `Changed("Parent")` -- and whichever settles second
-- must not emit a second `remove`.
local removedOnce: { [Instance]: boolean } = {}

-- Assigned below; declared here because `adoptShape` clears the id maps and
-- must drop the connections that went with them.
local disconnectInstances: () -> () = function() end

-- Assigned below: a checkpoint taken while the recorder is already running
-- numbers instances that were not there before, and those have to be observed
-- from the moment they get an id rather than at the next `Journal.start`.
local watchInstance: (Instance) -> () = function() end

-- Assigned below: handling a `Parent` change needs `settleParent` and the
-- removal batch, both of which are defined further down.
local onParentChanged: (Instance) -> () = function() end

type AddedRoot = { instance: Instance, parent: string, name: string, class: string, capturedAt: number, capturedTick: number }

local addedRoots: { [string]: AddedRoot } = {}

-- Ids this Studio handed to instances that appeared DURING the session, as
-- opposed to instances that were already there when it numbered. An add whose
-- parent is one of these travels inside that parent's blob and is not
-- serialized again; anything else is an added root of its own.
--
-- This used to be read off the id — `new:` meant "added since the checkpoint"
-- — and that stopped being true the moment a checkpoint could hand over ids
-- main had minted after an earlier one, or a clone could carry `new:` ids for
-- Studio's own migration extras. Both are instances that were there first.
local sessionAdded: { [string]: boolean } = {}
local blobs = Transfers.newOutStore({ ttl = Journal.BLOB_TTL_SECONDS, dropOnEof = false })

local endpoint: { host: string, port: string, https: boolean }? = nil
local recording: string? = nil

-- What has been recorded since the recorder started, by op. Purely for
-- `journal_status`: a live branch can be checked without reading the journal.
local counts: { [string]: number } = {}

local flushedTotal = 0
local lastFlushAt = 0
local looping = false

-- While the recorder is muted it neither emits nor allocates ids. `branch up`
-- replays a branch's own journal into its own clone (`own` mode below), and
-- those changes ARE the journal — recording them again would duplicate every
-- event the next flush pushes.
local muted = false

-- The id a replay is about to parent an instance under, consumed by the very
-- next `DescendantAdded`. A replay creates instances that ALREADY have an id —
-- the branch's — and the recorder must adopt that id instead of minting one of
-- its own and journalling main's own duplicate `add` for it.
local adoptingId: string? = nil

-- Set by `adoptShape`, which drops every watcher along with the ids they went
-- with. `Journal.start` no longer restarts a running recorder, so this is what
-- tells it the one case where it still has to re-observe.
local observationStale = false

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

local function assign(instance: Instance, id: string)
	idOf[instance] = id
	byId[id] = instance
	parentOf[instance] = parentIdOf(instance)
end

--- Where this Studio's minted ids come from. A branch clone passes its branch
--- name, so its `new:` ids are `new:<name>:<m>` and cannot collide with
--- another clone's or with main's. Main passes nothing.
function Journal.setOrigin(tag: string?)
	if tag == nil or tag == "" then
		idTag = nil
		return
	end

	-- Only what a branch name may hold, and nothing that could be read as a
	-- second separator.
	idTag = (string.gsub(tag, "[^%w%-%._]", "_"))
end

--- The next id for an instance nobody has numbered: `new:<m>`, tagged with
--- this Studio's origin when it has one.
local function mintNewId(): string
	newCounter += 1

	return if idTag == nil then `new:{newCounter}` else `new:{idTag}:{newCounter}`
end

--- The counter value inside a `new:` id, whichever form it takes, so a replay
--- can push this session's counter past an id it did not mint. nil for a
--- checkpoint id.
local function newIdNumber(id: string): number?
	return tonumber(string.match(id, "^new:(%d+)$") or string.match(id, "^new:[%w%-%._]+:(%d+)$"))
end

--- Raise the counters so neither space can ever hand out an id that is
--- already in use. Fed by a shape's explicit ids and by a replay.
local function reserveId(id: string)
	local plain = tonumber(string.match(id, "^%d+$"))

	if plain ~= nil then
		if plain > numberCounter then
			numberCounter = plain
		end

		return
	end

	local minted = newIdNumber(id)

	if minted ~= nil and minted > newCounter then
		newCounter = minted
	end
end

--- The compact tree shape a branch aligns against (SHAPE_VERSION 2):
---
---   { v = 2, count = <total>, services = {
---       { service = "Workspace", base = <ids before this service>,
---         nodes = { {name, class, childCount, id}, ... } }, ... } }
---
--- `nodes` is the service's walk in tree order, and `childCount` is what makes
--- the flat array reconstructible into a tree (pre-order: a node is followed
--- by its whole first subtree, then the next). Written by main in
--- `branch_checkpoint` and read by a clone in `branch_adopt`; the engine only
--- moves the bytes.
---
--- v1 carried `{name, class, childCount}` and left the id implicit: the node
--- at index `k` was id `base + k`. That only held while a checkpoint
--- renumbered main from scratch, which a second live branch made impossible —
--- main's ids are now stable and arbitrary, so every node states its own.
--- `adoptShape` still reads a v1 shape positionally, so a clone cut by an
--- older plugin adopts exactly as it did before.
Journal.SHAPE_VERSION = 2

--- The positional shape this plugin can still adopt.
Journal.SHAPE_VERSION_LEGACY = 1

export type ShapeService = { service: string, base: number, nodes: { { any } } }
export type Shape = { v: number, count: number, services: { ShapeService } }

--- Number the tree, incrementally. An instance that already has an id KEEPS
--- it — a checkpoint taken while a branch is live must not repoint a single id
--- in that branch's journal — and only instances without one are numbered,
--- continuing the counter so nothing is ever reused. Nothing else is dropped
--- either: the added roots, the replay ledgers and the `new:` counter all
--- belong to the session, not to one checkpoint.
---
--- Returns `{count, numbered}` — the size of the numbered tree and how many
--- ids this pass had to hand out — plus `shape` when `withShape` asks for it,
--- built by the SAME walk that numbers, so the two can never describe
--- different trees.
function Journal.checkpoint(
	services: { Instance }?,
	withShape: boolean?
): { count: number, numbered: number, shape: Shape? }
	local list = services or Journal.services()

	local n = 0
	local numbered = 0
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

			local existing = idOf[child]
			local id: string

			if existing ~= nil then
				id = existing
			else
				numberCounter += 1
				numbered += 1
				id = tostring(numberCounter)
				assign(child, id)

				-- A checkpoint on a Studio that is ALREADY recording adds ids
				-- mid-stream, and those instances are observed from here —
				-- `Journal.start` no longer restarts, so nothing else would
				-- connect them. The first checkpoint deliberately does not:
				-- connecting two signals on every instance of a 157k-instance
				-- place synchronously is what `observeAll` chunks away from
				-- the remote op's deadline.
				if active then
					watchInstance(child)
				end
			end

			if nodes == nil then
				collect(child, service, nil)
				continue
			end

			-- Appended before the recursion, so a node is followed by its whole
			-- subtree, and patched after it, once the count is known.
			local entry = { child.Name, child.ClassName, 0, id }
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

	return { count = n, numbered = numbered, shape = shape }
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

type ShapeNode = {
	index: number,
	name: string,
	class: string,
	size: number,
	--- v2's explicit id. nil in a v1 shape, where the id is `base + index`.
	id: string?,
	children: { ShapeNode },
}

--- Rebuild a service's flat pre-order array into a forest. Each node keeps its
--- flat index (its id, once `base` is added, in a v1 shape), its explicit id
--- when the shape carries one, and its subtree size (what a skipped node
--- costs in unused ids).
local function parseShapeNodes(nodes: { { any } }): { ShapeNode }
	local cursor = 1

	local function parseNode(): ShapeNode
		local entry = nodes[cursor]
		local carried = entry[4]
		local node: ShapeNode = {
			index = cursor,
			name = tostring(entry[1]),
			class = tostring(entry[2]),
			size = 1,
			id = if type(carried) == "string" and carried ~= "" then carried else nil,
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
	assign(instance, mintNewId())
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
			-- v2 states the id main actually holds; v1 left it implicit in the
			-- node's pre-order position, which only ever worked because a
			-- checkpoint renumbered main from 1.
			assign(live, node.id or tostring(base + node.index))
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

	disconnectInstances()
	observationStale = true

	table.clear(idOf)
	table.clear(byId)
	table.clear(parentOf)
	table.clear(addedRoots)
	table.clear(sessionAdded)
	table.clear(replayApplied)
	table.clear(replayCreated)

	newCounter = 0
	numberCounter = 0

	local byService: { [string]: ShapeService } = {}

	for _, entry in shape.services do
		byService[entry.service] = entry

		-- Every id main is handing over is spoken for before a single extra is
		-- minted: main's ids are no longer `1..count`, so the only safe
		-- high-water marks are the ones the shape itself states.
		for _, node in entry.nodes do
			local carried = node[4]

			if type(carried) == "string" then
				reserveId(carried)
			end
		end
	end

	if shape.v == Journal.SHAPE_VERSION_LEGACY then
		-- A positional shape hands out `base + index`, so the whole range it
		-- covers is taken whether or not this clone matched every node.
		reserveId(tostring(shape.count))
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
		table.clear(parentOf)

		newCounter = 0
		numberCounter = 0
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
	emitTick += 1

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

	counts[op] = (counts[op] or 0) + 1

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

		if now - root.capturedAt < Journal.BLOB_REFRESH_SECONDS or root.capturedTick == emitTick then
			continue
		end

		local blobId = Journal.captureBlob(root.instance)

		root.capturedAt = os.clock()
		root.capturedTick = emitTick

		if blobId == nil then
			continue
		end

		-- A refresh is NOT a second add. While the original is still pending it
		-- is patched in place (no new seq, no second event); once it has gone
		-- out, the only way to carry the new bytes is another `add` for the
		-- same id, and `update` marks it as the blob-only thing it is. The
		-- first live branch recorded exactly this as a duplicate `new:2`.
		local existing = pending[coalesceKey(id, "add", {})]

		if existing ~= nil then
			existing.blob = blobId
			existing.t = nowMillis()
			continue
		end

		emit(id, "add", {
			parent = root.parent,
			name = root.instance.Name,
			class = root.class,
			blob = blobId,
			update = true,
		})
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

--------------------------------------------------------------------------------
-- Observation
--------------------------------------------------------------------------------

--- EVERY registered instance is observed: `Changed` and `AttributeChanged` are
--- connected the moment it gets an id and dropped when it leaves the tree for
--- good. The earlier scheme connected only what Studio's Selection touched or
--- a plugin op wrote through, which made a script driving edits through
--- `wsync exec` — which is how agents actually edit — almost entirely
--- invisible: a `SetAttribute`, a rename and a reparent all went unrecorded.
---
--- Signals never emit directly. They mark the instance dirty and schedule one
--- drain per resumption cycle (`task.defer`); the drain reads the CURRENT
--- value once per property. A property written a thousand times in one frame
--- therefore costs one read and one event, and an idle journal costs nothing
--- at all — there is no polling connection to pay for.

--- Emit everything marked dirty since the last drain.
local function drainDirty()
	drainScheduled = false

	local work = dirty
	dirty = {}

	for instance, props in work do
		local id = idOf[instance]

		if id == nil then
			continue
		end

		local rec = dirtyRec[instance]
		dirtyRec[instance] = nil

		local previous = recording

		if rec ~= nil then
			recording = rec
		end

		for prop in props do
			if prop == "Parent" then
				onParentChanged(instance)
			elseif string.sub(prop, 1, 1) == "@" then
				local name = string.sub(prop, 2)

				local okRead, value = pcall(function()
					return instance:GetAttribute(name)
				end)

				emit(id, "attr", { name = name, value = if okRead then encodeAttribute(value) else nil })
			elseif prop == "Name" then
				emit(id, "rename", { name = instance.Name })
			else
				local encoded = Journal.encodeProperty(instance, prop)

				if encoded ~= nil then
					emit(id, "set", { prop = prop, value = encoded })
				end
			end
		end

		recording = previous
	end
end

local function scheduleDrain()
	if drainScheduled then
		return
	end

	drainScheduled = true

	task.defer(function()
		local ok, err = pcall(drainDirty)

		if not ok then
			drainScheduled = false
			Log.debug("Journal: drain failed:", err)
		end
	end)
end

local function markDirty(instance: Instance, prop: string)
	if not active or muted or idOf[instance] == nil then
		return
	end

	local props = dirty[instance]

	if props == nil then
		props = {}
		dirty[instance] = props
	end

	props[prop] = true

	scheduleDrain()
end

--- Connect one instance. Idempotent, and cheap enough to call on every add.
---
--- `Changed` on a `ValueBase` reports the new VALUE rather than a property
--- name (a long-standing engine quirk), so those get explicit per-property
--- signals instead.
local function watch(instance: Instance)
	if instanceConnections[instance] ~= nil then
		return
	end

	local list: { RBXScriptConnection } = {}

	if instance:IsA("ValueBase") then
		for _, prop in { "Name", "Value", "Parent" } do
			pcall(function()
				table.insert(
					list,
					instance:GetPropertyChangedSignal(prop):Connect(function()
						markDirty(instance, prop)
					end)
				)
			end)
		end
	else
		pcall(function()
			table.insert(
				list,
				instance.Changed:Connect(function(prop: string)
					-- A reparent INSIDE one service never fires that service's
					-- DescendantAdded/DescendantRemoving -- the instance never
					-- stops being a descendant of it -- so this is the ONLY
					-- signal that sees `ws.A.Parent = ws.B`.
					if prop == "Parent" then
						markDirty(instance, "Parent")
						return
					end

					if SKIP_PROPERTIES[prop] then
						return
					end

					-- git owns script bodies; the replay never touches them either.
					if prop == "Source" and instance:IsA("LuaSourceContainer") then
						return
					end

					markDirty(instance, prop)
				end)
			)
		end)
	end

	pcall(function()
		table.insert(
			list,
			instance.AttributeChanged:Connect(function(name: string)
				markDirty(instance, `@{name}`)
			end)
		)
	end)

	instanceConnections[instance] = list
	observed += 1
end

watchInstance = function(instance: Instance)
	watch(instance)
end

--- Drop an instance's connections. The id mapping is deliberately KEPT: events
--- already in the stream name it, and a merge has to be able to resolve them.
local function unwatch(instance: Instance)
	local list = instanceConnections[instance]

	if list == nil then
		return
	end

	for _, connection in list do
		pcall(function()
			connection:Disconnect()
		end)
	end

	instanceConnections[instance] = nil
	observed -= 1

	dirty[instance] = nil
	dirtyRec[instance] = nil
end

disconnectInstances = function()
	for instance in instanceConnections do
		unwatch(instance)
	end

	table.clear(instanceConnections)
	table.clear(dirty)
	table.clear(dirtyRec)
	table.clear(removalBatch)
	table.clear(removedOnce)

	observed = 0
	drainScheduled = false
	removalScheduled = false
end

--- Connect everything currently registered, in chunks, off the calling thread.
--- `journal_status` reports `observing` until it finishes; edits in that window
--- are the one gap this design has, and it is bounded by the size of the place
--- rather than by anything the user does.
local function observeAll()
	-- Snapshotted first: `watch` yields nothing, but a DescendantAdded landing
	-- mid-pass would add keys to `idOf` while it was being traversed.
	local roll: { Instance } = {}

	for instance in idOf do
		table.insert(roll, instance)
	end

	observing = true

	task.spawn(function()
		for index, instance in roll do
			if not active then
				break
			end

			watch(instance)

			if index % Journal.OBSERVE_CHUNK == 0 then
				task.wait()
			end
		end

		observing = false

		Log.stage(`Journal: observing {observed} instances`)
	end)
end

--- The write-handler hook (Handlers/Write calls this after every `set`,
--- `attr`, `tag`, `rename` and `move`). Observation no longer depends on it —
--- the instance is already connected — so all it does now is label whatever
--- that op dirtied with the op's own ChangeHistory name.
function Journal.touched(instance: Instance?, rec: string?)
	if not active or instance == nil or idOf[instance] == nil then
		return
	end

	watch(instance)

	if rec ~= nil then
		dirtyRec[instance] = rec
	end
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

--------------------------------------------------------------------------------
-- Structure
--------------------------------------------------------------------------------

local function inWatchedService(instance: Instance): boolean
	for service in watchedServices do
		if instance:IsDescendantOf(service) then
			return true
		end
	end

	return false
end

--- Emit a `move` when this instance's parent is not the one last recorded for
--- it, and remember the new one. Comparing ids rather than Instances is what
--- makes this independent of WHEN the engine completes a Parent assignment
--- relative to its own signals, and it also drops the ride-along: every
--- descendant of a moved subtree is re-added too, but only the root's parent
--- actually changed.
local function settleParent(instance: Instance): boolean
	local id = idOf[instance]

	if id == nil then
		return false
	end

	local parentId = parentIdOf(instance)

	if parentId == nil or parentId == parentOf[instance] then
		return false
	end

	parentOf[instance] = parentId
	removedOnce[instance] = nil
	emit(id, "move", { parent = parentId })

	return true
end

--- Is any ancestor of this instance also in this batch? Walked through
--- `parentOf`, because a destroyed instance no longer has a `Parent` to walk.
local function ancestorIsRemoved(instance: Instance): boolean
	local parentId = parentOf[instance]
	local guard = 0

	while parentId ~= nil and guard < 4096 do
		if string.sub(parentId, 1, 4) == "svc:" then
			return false
		end

		local parent = byId[parentId]

		if parent == nil then
			return false
		end

		if removalBatch[parent] then
			return true
		end

		parentId = parentOf[parent]
		guard += 1
	end

	return false
end

--- Settle one cycle's worth of removals together. Destroying a Model with 181
--- descendants fires 182 `DescendantRemoving`s; only the root is worth
--- recording, because a replay that destroys the root takes the subtree with
--- it and already treats a missing instance as gone. Doing this per batch
--- rather than per instance is also what lets a reparent cancel itself: the
--- `DescendantAdded` runs before this does.
local function settleRemovals()
	removalScheduled = false

	local batch = removalBatch
	removalBatch = {}

	local gone: { Instance } = {}

	for instance in batch do
		if idOf[instance] == nil then
			continue
		end

		if inWatchedService(instance) then
			-- Still in the tree: this was a move, not a removal. Usually the
			-- DescendantAdded already settled it, in which case this is a no-op.
			settleParent(instance)
			continue
		end

		table.insert(gone, instance)
	end

	-- `ancestorIsRemoved` reads the batch, so it has to see all of it.
	for _, instance in gone do
		removalBatch[instance] = true
	end

	for _, instance in gone do
		if not ancestorIsRemoved(instance) then
			emit(idOf[instance] :: string, "remove", {})
		end
	end

	for _, instance in gone do
		-- Marked whether or not it got its own event: a descendant folded into
		-- its ancestor's removal is just as gone, and must not come back
		-- through the other path.
		removedOnce[instance] = true
		removalBatch[instance] = nil
		unwatch(instance)
	end
end

local function onDescendantAdded(instance: Instance, service: Instance)
	if muted or Journal.isExcluded(instance, service) then
		return
	end

	if adoptingId ~= nil and idOf[instance] == nil then
		-- The root a replay is parenting right now. It already HAS an id — the
		-- one the branch's journal gave it — so minting a second one for it and
		-- journalling an `add` would put main's own duplicate of the branch's
		-- instance into main's stream, and leave `byId` holding two ids for one
		-- instance.
		local id = adoptingId :: string
		adoptingId = nil

		assign(instance, id)
		watch(instance)

		-- It did appear in this session, so everything the replay puts under
		-- it belongs to its subtree rather than being a root of its own.
		sessionAdded[id] = true

		return
	end

	local existing = idOf[instance]

	if existing ~= nil then
		-- Already registered, so this is the second half of a reparent — which
		-- may be within one service or across two. Cancel the pending removal
		-- and settle the parent now, whichever it was.
		removalBatch[instance] = nil

		watch(instance)
		settleParent(instance)

		return
	end

	local parent = parentIdOf(instance)

	if parent == nil then
		return
	end

	local id = mintNewId()
	assign(instance, id)
	watch(instance)
	sessionAdded[id] = true

	local class = instance.ClassName

	-- An *added root* is one whose parent was already there when this Studio
	-- numbered: everything under it travels inside its blob, so only the root
	-- is serialized and re-serialized.
	local isRoot = not sessionAdded[parent]

	local blobId: string? = nil

	if isRoot then
		blobId = Journal.captureBlob(instance)

		addedRoots[id] = {
			instance = instance,
			parent = parent,
			name = instance.Name,
			class = class,
			capturedAt = os.clock(),
			capturedTick = emitTick + 1,
		}
	end

	emit(id, "add", { parent = parent, name = instance.Name, class = class, blob = blobId })
end

--- Put an instance in line to be settled as a removal. Nothing is decided
--- here: `instance.Parent` is not reliably the old parent at DescendantRemoving
--- time, so the whole cycle is settled together, once everything that might
--- cancel a removal has run.
local function queueRemoval(instance: Instance)
	if muted or idOf[instance] == nil or removedOnce[instance] then
		return
	end

	removalBatch[instance] = true

	if removalScheduled then
		return
	end

	removalScheduled = true

	task.defer(function()
		local ok, err = pcall(settleRemovals)

		if not ok then
			removalScheduled = false
			Log.debug("Journal: removal settle failed:", err)
		end
	end)
end

local function onDescendantRemoving(instance: Instance)
	queueRemoval(instance)
end

--- An instance's own `Parent` changed. This is the only signal that sees a
--- move within one service, and it also sees a destroy (Parent goes nil)
--- before the service's DescendantRemoving does -- `queueRemoval` and
--- `removedOnce` are what keep the two paths from double-counting.
onParentChanged = function(instance: Instance)
	if muted or idOf[instance] == nil then
		return
	end

	if not inWatchedService(instance) then
		queueRemoval(instance)
		return
	end

	settleParent(instance)
end

local function startLoop()
	if looping then
		return
	end

	looping = true

	task.spawn(function()
		local lastFlush = os.clock()

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
	table.clear(subscribedTags)
	table.clear(watchedServices)

	disconnectInstances()
end

--- Start recording against a checkpoint. The tree must already be numbered
--- (`Journal.checkpoint`).
---
--- A Studio that is ALREADY recording keeps the stream it has. Main takes a
--- checkpoint per branch, and restarting the stream at the second one cut the
--- first branch off from every edit main made afterwards — which is precisely
--- what that branch's conflict detection reads. So the stream, its ids, its
--- history and its `new:` counter all belong to the session, and a later
--- checkpoint only adds a mark into the one stream (`Journal.mark`).
function Journal.start(checkpoint: string, services: { Instance }?)
	if active then
		if observationStale then
			-- An `adoptShape` dropped every watcher with the ids they went
			-- with. Nothing but a re-adopt on a live clone gets here, and it
			-- must not leave the tree unobserved.
			observationStale = false
			observeAll()
		end

		if checkpointId ~= checkpoint then
			Log.stage(
				`Journal: checkpoint {checkpoint} joins the session stream {tostring(checkpointId)} `
					.. `({checkpointCount} instances numbered)`
			)
		end

		return
	end

	disconnectAll()

	active = true
	checkpointId = checkpoint

	local list = services or Journal.services()

	for _, service in list do
		watchedServices[service] = true

		table.insert(
			connections,
			service.DescendantAdded:Connect(function(instance: Instance)
				onDescendantAdded(instance, service)
			end)
		)

		table.insert(connections, service.DescendantRemoving:Connect(onDescendantRemoving))
	end

	-- Everything already numbered gets connected too. Detached and chunked:
	-- see observeAll.
	observeAll()

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
--- There is one stream and one mark per live branch; a mark is never moved,
--- because the events before it are exactly the ones that branch already has.
function Journal.mark(checkpoint: string)
	marks[checkpoint] = #history
end

--- Every checkpoint main has marked in this session, with how many of main's
--- own events it has seen since. `journal_status` reports it so two live
--- branches can be told apart without reading the journal.
function Journal.marks(): { { checkpoint: string, since: number } }
	local list: { { checkpoint: string, since: number } } = {}

	for checkpoint, at in marks do
		table.insert(list, { checkpoint = checkpoint, since = #history - at })
	end

	table.sort(list, function(a, b)
		return a.checkpoint < b.checkpoint
	end)

	return list
end

function Journal.status(): { [string]: any }
	return {
		active = active,
		branch = Branch.isBranch(),
		checkpoint = checkpointId,
		count = checkpointCount,
		pending = pendingCount,
		observed = observed,
		observing = observing,
		adds = counts.add or 0,
		removes = counts.remove or 0,
		moves = counts.move or 0,
		renames = counts.rename or 0,
		sets = counts.set or 0,
		attrs = counts.attr or 0,
		tags = counts.tag or 0,
		flushed = flushedTotal,
		seq = seq,
		lastFlushAt = lastFlushAt,
		connected = endpoint ~= nil,
		origin = idTag,
		shapeVersion = Journal.SHAPE_VERSION,
		numbered = numberCounter,
		minted = newCounter,
		marks = Journal.marks(),
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
		-- An `add` for an id that already maps to an instance this replay
		-- created is NEVER a second instance: it is either a blob refresh
		-- (`update = true` marks the deliberate ones) or the same journal being
		-- replayed a second time after a `pick`.
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

		-- Claimed before the parenting, because `DescendantAdded` fires inside
		-- it: the recorder adopts this id rather than minting one of its own
		-- and journalling main's own duplicate `add`.
		adoptingId = event.id

		local ok = pcall(function()
			added.Name = event.name
			added.Parent = parent
		end)

		adoptingId = nil

		if not ok then
			return false
		end

		created[event.id] = added

		-- Registered under the id the journal gave it, so every later event in
		-- the same stream resolves. The session counters are pushed past it so
		-- a live add after the replay cannot reuse the number.
		assign(added, event.id)
		reserveId(event.id)

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

		local moved = pcall(function()
			instance.Parent = parent
		end)

		if moved then
			parentOf[instance] = event.parent
		end

		return moved
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
--- `decisions = {[n] = "ours" | "theirs"}` applies the chosen side — and ONLY
--- that side: a replay is idempotent per checkpoint, so the second run reruns
--- nothing the first one already did. It reports what it re-recognised as
--- `alreadyApplied`.
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

	-- Both ledgers persist across calls for this checkpoint (see their
	-- declarations): `created` so a `new:` id keeps meaning the instance the
	-- first run made, `done` so nothing is applied twice.
	local created = replayCreated[checkpoint]

	if created == nil then
		created = {}
		replayCreated[checkpoint] = created
	end

	local done = replayApplied[checkpoint]

	if done == nil then
		done = {}
		replayApplied[checkpoint] = done
	end

	local conflicts: { Conflict } = {}
	local applied = 0
	local skipped = 0
	local alreadyApplied = 0

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

			-- The branch's own sequence number is the stable identity of an
			-- event across reruns; the array index is only a fallback, kept
			-- negative so it can never collide with a real seq.
			local stamp = if type(event.seq) == "number" then event.seq else -n

			if done[stamp] then
				alreadyApplied += 1
				continue
			end

			if event.op == "add" and created[event.id] ~= nil then
				done[stamp] = true
				alreadyApplied += 1
				continue
			end

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
				-- A decision is a resolution: the event is settled, so a later
				-- run must not park it as a conflict all over again.
				done[stamp] = true
				skipped += 1
				continue
			end

			if applyEvent(event, instance, blobsIn, created) then
				done[stamp] = true
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
		alreadyApplied = alreadyApplied,
		conflicts = conflicts,
		own = reconstructing,
		stopped = if ok then nil else tostring(caught),
	}
end

return Journal
