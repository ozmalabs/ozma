-- ozma-routing.lua — WirePlumber audio routing policy for Ozma.
--
-- Watches the "ozma" PipeWire metadata namespace for the "active_node" key.
-- When set, links that node's output ports to the default audio output sink.
-- When cleared (empty string), all ozma-managed links are destroyed.
--
-- Controller signals a switch with:
--   pw-metadata -n ozma set 0 active_node ozma-vm1
-- Controller signals disconnect with:
--   pw-metadata -n ozma set 0 active_node ""
--
-- Install: see controller/wireplumber/install.sh

local METADATA_NAME = "ozma"
local METADATA_KEY  = "active_node"

-- Currently managed link proxy objects (destroyed on each switch)
local managed_links = {}

-- Current active source name (to detect no-op re-activations)
local active_source = ""

-- Port ObjectManager — tracks all non-physical ports
local port_om = ObjectManager {
  Interest {
    type = "port",
    Constraint { "port.physical", "!", "true", type = "pw-global" },
  },
}

-- Node ObjectManager — tracks all audio nodes
local node_om = ObjectManager {
  Interest {
    type = "node",
    Constraint { "media.class", "#", "Audio/*", type = "pw-global" },
  },
}

-- ── Helpers ──────────────────────────────────────────────────────────────────

-- Return the bound PipeWire ID (integer) for a proxy object.
local function bound_id(proxy)
  return proxy["bound-id"]
end

-- Find a node by its node.name property. Returns the node proxy or nil.
local function find_node_by_name(name)
  for node in node_om:iterate() do
    if node.properties["node.name"] == name then
      return node
    end
  end
  return nil
end

-- Find all ports belonging to a node ID with the given direction ("out"/"in").
local function find_ports(node_id_str, direction)
  local ports = {}
  for port in port_om:iterate() do
    local p = port.properties
    if p["node.id"] == node_id_str and p["port.direction"] == direction then
      table.insert(ports, port)
    end
  end
  return ports
end

-- Find the default output sink node (media.class = Audio/Sink, not a monitor).
-- Returns the node proxy or nil.
local function find_output_sink()
  -- Prefer the WirePlumber default node metadata if available.
  -- Fallback: first Audio/Sink that isn't a null-sink monitor source.
  local default_name = nil

  -- Try to read the WirePlumber default-nodes metadata
  local wp_meta = ImplMetadata.find("default")
  if wp_meta then
    default_name = wp_meta:find("default.audio.sink", 0)
  end

  if default_name and default_name ~= "" then
    local n = find_node_by_name(default_name)
    if n then return n end
  end

  -- Fallback: first non-monitor Audio/Sink
  for node in node_om:iterate() do
    local mc = node.properties["media.class"] or ""
    local name = node.properties["node.name"] or ""
    if mc == "Audio/Sink" and not name:match("%.monitor$") then
      return node
    end
  end
  return nil
end

-- ── Link management ──────────────────────────────────────────────────────────

local function destroy_managed_links()
  for _, link in ipairs(managed_links) do
    pcall(function() link:request_destroy() end)
  end
  managed_links = {}
end

local function create_link(out_port, in_port)
  local out_nid = out_port.properties["node.id"]
  local in_nid  = in_port.properties["node.id"]
  local out_pid = tostring(bound_id(out_port))
  local in_pid  = tostring(bound_id(in_port))

  local link = Core.create_object("PipeWire:Interface:Link", {
    ["link.output.node"] = out_nid,
    ["link.output.port"] = out_pid,
    ["link.input.node"]  = in_nid,
    ["link.input.port"]  = in_pid,
    ["object.linger"]    = "true",
  })
  if link then
    table.insert(managed_links, link)
    Log.info(string.format("ozma: linked port %s→%s", out_pid, in_pid))
  end
end

local function route_to_sink(source_name, sink_node)
  local source_node = find_node_by_name(source_name)
  if not source_node then
    Log.warning("ozma: source node not found: " .. source_name)
    return
  end

  local src_id  = tostring(bound_id(source_node))
  local sink_id = tostring(bound_id(sink_node))

  local out_ports = find_ports(src_id,  "out")
  local in_ports  = find_ports(sink_id, "in")

  if #out_ports == 0 then
    Log.warning("ozma: no output ports on source " .. source_name)
    return
  end
  if #in_ports == 0 then
    Log.warning("ozma: no input ports on sink " .. (sink_node.properties["node.name"] or "?"))
    return
  end

  -- Build channel map for sink input ports
  local in_by_channel = {}
  for _, p in ipairs(in_ports) do
    local ch = p.properties["audio.channel"] or p.properties["port.alias"] or ""
    in_by_channel[ch] = p
  end

  for _, out_p in ipairs(out_ports) do
    local ch = out_p.properties["audio.channel"] or out_p.properties["port.alias"] or ""
    local in_p = in_by_channel[ch] or in_ports[1]
    if in_p then
      create_link(out_p, in_p)
    end
  end
end

local function activate_node(source_name)
  if source_name == active_source then return end
  active_source = source_name

  destroy_managed_links()

  if source_name == "" then
    Log.info("ozma: audio disconnected")
    return
  end

  local sink = find_output_sink()
  if not sink then
    Log.warning("ozma: no output sink found — will retry when nodes change")
    -- The node_om object-added hook will retry
    return
  end

  Log.info(string.format("ozma: routing %s → %s",
    source_name, sink.properties["node.name"] or "?"))
  route_to_sink(source_name, sink)
end

-- ── Metadata watcher ─────────────────────────────────────────────────────────

SimpleEventHook {
  name = "ozma/audio-routing",
  interests = {
    EventInterest {
      Constraint { "event.type",        "=", "metadata-changed" },
      Constraint { "metadata.name",     "=", METADATA_NAME },
      Constraint { "event.subject.key", "=", METADATA_KEY },
    },
  },
  execute = function(event)
    local val = event:get_data("metadata.value") or ""
    -- Metadata value may be JSON-encoded: {"value":"ozma-vm1"}
    -- Strip if it looks like that.
    if val:match('^%s*{') then
      val = val:match('"value"%s*:%s*"([^"]*)"') or ""
    end
    activate_node(val)
  end,
}:register()

-- Retry routing when a new node appears (handles startup race where the
-- source or sink node wasn't in the ObjectManager yet when metadata was set).
node_om:connect("object-added", function(_, node)
  if active_source ~= "" and #managed_links == 0 then
    Log.info("ozma: new node appeared, retrying routing for " .. active_source)
    local pending = active_source
    active_source = ""          -- reset so activate_node doesn't skip it
    activate_node(pending)
  end
end)

-- Create the "ozma" metadata namespace so the controller can write to it.
-- If WirePlumber already created it, this is a no-op.
local _ozma_meta = ImplMetadata(METADATA_NAME)

om_start_all = function()
  port_om:activate()
  node_om:activate()
end

om_start_all()

Log.info("ozma-routing.lua loaded — waiting for pw-metadata -n ozma set 0 active_node <name>")
