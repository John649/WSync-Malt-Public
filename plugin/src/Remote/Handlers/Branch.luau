--!strict
-- Branch ops (docs/branches.md). Six ops, three of them main-side:
--
--   branch_checkpoint {name}        main   serialize each content service,
--                                          then number the tree in the same
--                                          op so blobs and numbering agree,
--                                          and pin a shape of that numbering;
--                                          starts main's own recorder and
--                                          marks it for this branch
--   branch_adopt {checkpoint, count, branch align the freshly loaded clone
--                 shapeBlob?}               against main's shape (or, without
--                                          shapeBlob, against its count) and
--                                          start the journal
--   journal_flush {}                both   push everything pending now
--   journal_status {}               both   what the recorder is holding
--   journal_replay {checkpoint,     main   apply a branch's stream inside one
--                   events, blobs,         ChangeHistory recording and report
--                   decisions}             the conflicts it skipped
--   journal_replay {..., own = true} branch `branch up`: the clone replaying its
--                                          OWN journal back into itself — no
--                                          conflict detection, everything
--                                          applies, recorder muted throughout
--
-- and the blob pull, modelled on `clipboard_copy` -> `clipboard_read`:
--
--   branch_blob_open  {artifactId}          -> {bytes, sha256}
--   branch_blob_read  {artifactId, offset,  -> {data, eof, totalBytes}
--                      maxLen}
--   branch_blob_close {artifactId}          -> {ok}
--
-- These are NOT aliases over the clipboard ops: a journal blob is pinned in
-- the journal's own store (Journal.captureBlobBytes), not in the clipboard's,
-- because a clipboard read drops its entry at EOF and a copy atomically
-- replaces the previous clip — both wrong for a checkpoint whose eleven
-- service blobs are pulled one after another. The store keeps the serialized
-- bytes per artifactId until `branch_blob_close` drops them, or ten minutes
-- of idleness retire them (Journal.BLOB_TTL_SECONDS).
--
-- One id space serves both kinds of blob: an `add` event's subtree and a
-- `branch_checkpoint` service blob are pinned the same way and pulled with
-- the same three ops. `journal_blob_read` is kept as a name for
-- `branch_blob_read` so the journal side reads as its own thing.
--
-- `journal_replay` takes blobs the other way round, as
-- `{[blobId] = <base64 rbxm>}` in the request, because main's plugin has
-- nowhere to pull a branch daemon's artifact from.

local HttpService = game:GetService("HttpService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local Base64 = require(WSync.Lib.Base64)
local Branch = require(WSync.Branch)
local Journal = require(WSync.Journal)

local Error = require(script.Parent.Parent.Error)

type Args = { [string]: any }

local BranchOps = {}

BranchOps.LIMITS = {
	maxNameBytes = 128,
	readMaxLen = Journal.BLOB_READ_MAX_LEN,
}

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

BranchOps.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--------------------------------------------------------------------------------
-- branch_checkpoint (main)
--------------------------------------------------------------------------------

--- `branch_checkpoint {name}` → `{checkpoint, count, blobs}`.
---
--- Order matters and is pinned: every content service's children are
--- serialized FIRST, then the tree is numbered in the same op, so the blobs
--- the branch clone is built from and the numbering both sides agree on
--- describe the same tree. Numbering after a yielding serialize would let an
--- edit land between the two and give the branch a count main never had.
function BranchOps.ops.branch_checkpoint(args: Args, _ctx)
	local name = args.name

	if type(name) ~= "string" or name == "" or #name > BranchOps.LIMITS.maxNameBytes then
		invalid(`name must be a string of 1-{BranchOps.LIMITS.maxNameBytes} bytes`)
	end

	local service = serializationService()
	local services = Journal.services()

	-- `blobs` carries the full descriptor the CLI needs to pull without an
	-- `open` round trip; `blobIds` is the flat `{Service = artifactId}` the
	-- original checkpoint contract pinned, kept so either reader works.
	local blobs: { [string]: { [string]: any } } = {}
	local blobIds: { [string]: string } = {}

	for _, content in services do
		local roots: { Instance } = {}

		for _, child in content:GetChildren() do
			if not Journal.isExcluded(child, content) then
				table.insert(roots, child)
			end
		end

		if #roots == 0 then
			continue
		end

		local ok, bytes = pcall(function()
			return service:SerializeInstancesAsync(roots)
		end)

		if not ok or typeof(bytes) ~= "buffer" then
			pluginError(`could not serialize '{content.Name}': {tostring(bytes)}`)
		end

		local blobId = Journal.captureBlobBytes(bytes :: buffer)

		if blobId == nil then
			pluginError(`the journal blob store rejected '{content.Name}'`)
		end

		local info = Journal.blobInfo(blobId :: string)

		blobs[content.Name] = {
			artifactId = blobId,
			bytes = if info ~= nil then info.bytes else buffer.len(bytes :: buffer),
			sha256 = if info ~= nil then info.sha256 else nil,
		}

		blobIds[content.Name] = blobId :: string
	end

	local numbered = Journal.checkpoint(services, true)

	-- The shape is what a clone aligns against. Studio adds instances of its
	-- own when it opens a place file, so the clone can never reproduce main's
	-- count exactly; it can reproduce main's *tree*, and that is what this
	-- describes.
	local encoded = nil

	if numbered.shape ~= nil then
		local okEncode, json = pcall(function()
			return HttpService:JSONEncode(numbered.shape)
		end)

		if not okEncode then
			pluginError(`could not encode the checkpoint shape: {tostring(json)}`)
		end

		local shapeId = Journal.captureBlobBytes(buffer.fromstring(json :: string))

		if shapeId == nil then
			pluginError("the journal blob store rejected the checkpoint shape")
		end

		local shapeInfo = Journal.blobInfo(shapeId :: string)

		encoded = {
			artifactId = shapeId,
			bytes = if shapeInfo ~= nil then shapeInfo.bytes else #(json :: string),
			sha256 = if shapeInfo ~= nil then shapeInfo.sha256 else nil,
		}
	end

	local checkpoint = HttpService:GenerateGUID(false)

	-- Main records its own stream from here, with a mark this branch's merge
	-- reads back ("Main keeps one journal with a checkpoint mark per branch").
	Journal.start(checkpoint, services)
	Journal.mark(checkpoint)

	Log.stage(`Branch checkpoint '{name}': {numbered.count} instances, {checkpoint}`)

	return {
		checkpoint = checkpoint,
		count = numbered.count,
		blobs = blobs,
		blobIds = blobIds,
		shape = encoded,
		shapeId = if encoded ~= nil then encoded.artifactId else nil,
	}
end

--------------------------------------------------------------------------------
-- branch_adopt (branch clone)
--------------------------------------------------------------------------------

--- `branch_adopt {checkpoint, count, shapeBlob?}` →
--- `{ok, matched, extras, missing, count, ...}`.
---
--- With `shapeBlob`, the clone adopts main's numbering by SHAPE: it pulls the
--- shape from its own daemon (`GET /journal/blob/<id>`) and aligns the live
--- tree against it parent by parent, in order, by `(name, class)` with skips.
--- This is not a refinement — it is the only thing that can work. Studio adds
--- instances of its own while it opens a place file (a Lighting
--- post-processing child, for one), so a clone's count is reliably main's
--- count plus a few and an exact-count gate refuses every real branch.
---
--- A live child with no counterpart is a migration extra: it takes a `new:`
--- id so events about it resolve, and it is never journalled as an `add`
--- because nobody added it. A shape child with no live counterpart simply
--- leaves its id unused. The adoption is refused when it matched less than
--- `ALIGN_MIN_RATIO` of main's nodes, or when any service main described
--- matched nothing at all — a tree that loose is not the same tree.
---
--- Without `shapeBlob` the old exact-count behaviour is kept verbatim.
function BranchOps.ops.branch_adopt(args: Args, _ctx)
	local checkpoint = args.checkpoint

	if type(checkpoint) ~= "string" or checkpoint == "" then
		invalid("checkpoint must be a non-empty string")
	end

	local expected = args.count

	if type(expected) ~= "number" or expected % 1 ~= 0 or expected < 0 then
		invalid("count must be a non-negative integer")
	end

	if args.shapeBlob ~= nil and (type(args.shapeBlob) ~= "string" or args.shapeBlob == "") then
		invalid("shapeBlob must be a non-empty artifact id")
	end

	local services = Journal.services()

	if args.shapeBlob == nil then
		local numbered = Journal.checkpoint(services)

		if numbered.count ~= expected then
			Log.warn(`Branch adopt refused: numbered {numbered.count}, main numbered {expected}`)

			return { ok = false, count = numbered.count, expected = expected }
		end

		Journal.start(checkpoint, services)

		return { ok = true, count = numbered.count }
	end

	local body, reason = Journal.fetchBlob(args.shapeBlob)

	if body == nil then
		error(Error.new(Error.PLUGIN_ERROR, reason or "the checkpoint shape could not be fetched"), 0)
	end

	local okDecode, shape = pcall(function()
		return HttpService:JSONDecode(body :: string)
	end)

	if not okDecode or type(shape) ~= "table" or type(shape.services) ~= "table" then
		invalid(`the checkpoint shape in {args.shapeBlob} is not a readable shape document`)
	end

	if shape.v ~= Journal.SHAPE_VERSION then
		invalid(`the checkpoint shape is version {tostring(shape.v)}; this plugin speaks {Journal.SHAPE_VERSION}`)
	end

	if type(shape.count) ~= "number" then
		shape.count = expected
	end

	local result = Journal.adoptShape(shape, services)

	if result.ok ~= true then
		Log.warn(
			`Branch adopt refused: matched {result.matched}/{result.shapeCount} `
				.. `({result.extras} extras, {result.missing} missing)`
		)

		return result
	end

	Journal.start(checkpoint, services)

	Log.stage(
		`Branch adopt {checkpoint}: matched {result.matched}/{result.shapeCount}, `
			.. `{result.extras} extras, {result.missing} missing`
	)

	return result
end

--------------------------------------------------------------------------------
-- journal_flush / journal_status
--------------------------------------------------------------------------------

--- `journal_flush {}` → `{flushed, pending}`.
function BranchOps.ops.journal_flush(_args: Args, _ctx)
	return Journal.flush()
end

--- `journal_status {}` → what the recorder is holding.
function BranchOps.ops.journal_status(_args: Args, _ctx)
	local status = Journal.status()
	local identity = Branch.current()

	if identity ~= nil then
		status.name = identity.name
		status.syntheticPlaceId = identity.syntheticPlaceId
	end

	return status
end

--------------------------------------------------------------------------------
-- branch_blob_open / branch_blob_read / branch_blob_close
--------------------------------------------------------------------------------

local function validateArtifactId(args: Args): string
	local artifactId = args.artifactId or args.blobId

	if type(artifactId) ~= "string" or artifactId == "" then
		invalid("artifactId must be a non-empty string")
	end

	return artifactId
end

local function notFound(artifactId: string): never
	error(
		Error.new(
			Error.NOT_FOUND,
			`no blob {artifactId} (closed, expired, or never captured) — re-run branch_checkpoint`
		),
		0
	)
end

--- `branch_blob_open {artifactId}` → `{bytes, sha256}`: what a pull needs to
--- size and verify itself. Nothing is created here — the blob was pinned when
--- it was serialized — so `open` is a lookup that also refreshes the entry's
--- idle timer. `branch_checkpoint` already returns both fields per service, so
--- the CLI can skip this call entirely.
function BranchOps.ops.branch_blob_open(args: Args, _ctx)
	local artifactId = validateArtifactId(args)
	local info = Journal.blobInfo(artifactId)

	if info == nil then
		notFound(artifactId)
	end

	return { bytes = (info :: any).bytes, sha256 = (info :: any).sha256 }
end

--- `branch_blob_read {artifactId, offset, maxLen}` →
--- `{data, eof, totalBytes}`: one bounded base64 part, the shape
--- `clipboard_read` returns. Unlike a clipboard read the entry survives its
--- own EOF — an `add` blob is re-read after a re-capture, and a checkpoint
--- blob is dropped by `branch_blob_close`, not by the part that finished it.
--- `offset` and `maxLen` default to 0 and the maximum.
local function readBlobPart(args: Args)
	local artifactId = validateArtifactId(args)

	local offset = 0

	if args.offset ~= nil then
		if type(args.offset) ~= "number" or args.offset < 0 or args.offset % 1 ~= 0 then
			invalid("offset must be a non-negative integer")
		end

		offset = args.offset
	end

	local maxLen = BranchOps.LIMITS.readMaxLen

	if args.maxLen ~= nil then
		if
			type(args.maxLen) ~= "number"
			or args.maxLen < 1
			or args.maxLen > BranchOps.LIMITS.readMaxLen
			or args.maxLen % 1 ~= 0
		then
			invalid(`maxLen must be an integer of 1-{BranchOps.LIMITS.readMaxLen}`)
		end

		maxLen = args.maxLen
	end

	local part = Journal.readBlob(artifactId, offset, maxLen)

	if part == nil then
		notFound(artifactId)
	end

	return {
		data = Base64.encode((part :: any).data),
		eof = (part :: any).eof,
		totalBytes = (part :: any).totalBytes,
	}
end

function BranchOps.ops.branch_blob_read(args: Args, _ctx)
	return readBlobPart(args)
end

--- The journal's own name for the same pull: an `add` event's `blob` is an
--- artifact in the same store, read the same way.
function BranchOps.ops.journal_blob_read(args: Args, _ctx)
	return readBlobPart(args)
end

--- `branch_blob_close {artifactId}` → `{ok}`. `ok` is false when there was
--- nothing left to drop, so a double close is not an error.
function BranchOps.ops.branch_blob_close(args: Args, _ctx)
	local artifactId = validateArtifactId(args)

	return { ok = Journal.dropBlob(artifactId) }
end

--------------------------------------------------------------------------------
-- journal_replay (main)
--------------------------------------------------------------------------------

--- `journal_replay {checkpoint, events, blobs?, decisions?}` →
--- `{applied, skipped, conflicts}`, conflicts being
--- `[{n, id, path, kind, ours, theirs}]` with `kind` one of `remove`, `move`
--- or `touch`. Call it again with `decisions = {[n] = "ours"|"theirs"}` to
--- apply the chosen side of the ones that came back.
function BranchOps.ops.journal_replay(args: Args, ctx)
	local checkpoint = args.checkpoint

	if type(checkpoint) ~= "string" or checkpoint == "" then
		invalid("checkpoint must be a non-empty string")
	end

	local events = args.events

	if type(events) ~= "table" then
		invalid("events must be an array of journal events")
	end

	if args.blobs ~= nil and type(args.blobs) ~= "table" then
		invalid("blobs must be an object of {blobId = <base64 rbxm>}")
	end

	if args.decisions ~= nil and type(args.decisions) ~= "table" then
		invalid("decisions must be an object of {n = 'ours'|'theirs'}")
	end

	if args.own ~= nil and type(args.own) ~= "boolean" then
		invalid("own must be a boolean")
	end

	local own = args.own == true

	if Branch.isBranch() then
		-- The one replay a branch clone accepts is its own journal replayed
		-- back into itself (`branch up` reconstructing the branch from the
		-- clone plus the journal). Any other stream is a cross-Studio merge,
		-- which only main performs.
		if not own then
			error(
				Error.new(Error.PERMISSION_REQUIRED, "journal_replay runs on main — this Studio is a branch clone"),
				0
			)
		end

		local adopted = Journal.currentCheckpoint()

		if adopted == nil then
			error(Error.new(Error.CONFLICT, "this clone has adopted no checkpoint yet — run branch_adopt first"), 0)
		end

		if adopted ~= checkpoint then
			error(
				Error.new(
					Error.CONFLICT,
					`own:true replays this clone's own journal, but it adopted checkpoint {adopted}, not {checkpoint}`
				),
				0
			)
		end
	elseif own then
		error(Error.new(Error.INVALID_ARGUMENT, "own:true is the branch-up path; this Studio is main"), 0)
	end

	local result = Journal.replay(checkpoint, events, args.blobs, args.decisions, own)

	ctx.audit({
		op = "journal_replay",
		target = checkpoint,
		count = result.applied,
	})

	Log.stage(
		`Journal {if own then "restore" else "replay"} {checkpoint}: {result.applied} applied, `
			.. `{result.skipped} skipped, {#result.conflicts} conflicts`
	)

	return result
end

return BranchOps
