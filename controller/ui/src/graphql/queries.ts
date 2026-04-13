import type { NodeInfo } from '../types/node'
import type { Scenario } from '../store/ozmaStore'

// ── Response types ────────────────────────────────────────────────────────────

export interface GetNodesData {
  nodes: NodeInfo[]
  activeNodeId: string | null
}

export interface GetScenariosData {
  scenarios: Scenario[]
  activeScenarioId: string | null
}

export interface GetNodeByIdData {
  node: NodeInfo | null
}

export interface GetActiveNodeData {
  activeNode: NodeInfo | null
}

export interface ActivateNodeData {
  activateNode: { ok: boolean; activeNodeId: string }
}

export interface GetSystemSnapshotData {
  snapshot: { nodes: NodeInfo[]; activeNodeId: string | null }
}

// ── Shared node field list ────────────────────────────────────────────────────

const NODE_FIELDS = /* GraphQL */ `
  id
  name
  hostname
  host
  port
  role
  hw
  fw_version
  proto_version
  capabilities
  machine_class
  last_seen
  status
  active
  uptime_seconds
  ip_address
  mac_address
  platform
  version
  vnc_host
  vnc_port
  stream_port
  stream_path
  audio_type
  audio_sink
  audio_vban_port
  mic_vban_port
  capture_device
  camera_streams {
    name
    rtsp_inbound
    backchannel
    hls
  }
  frigate_host
  frigate_port
  owner_user_id
  owner_id
  shared_with
  share_permissions
  parent_node_id
  sunshine_port
  seat_count
  seat_config
  display_outputs {
    index
    source_type
    capture_source_id
    width
    height
  }
  scenario {
    id
    name
    color
  }
  hid_stats {
    total_keys
    total_clicks
    total_scrolls
    last_activity
  }
`

// ── Query documents ───────────────────────────────────────────────────────────

export const GET_NODES = /* GraphQL */ `
  query GetNodes {
    nodes {
      ${NODE_FIELDS}
    }
    activeNodeId
  }
`

export const GET_NODE_BY_ID = /* GraphQL */ `
  query GetNodeById($id: ID!) {
    node(id: $id) {
      ${NODE_FIELDS}
    }
  }
`

export const GET_ACTIVE_NODE = /* GraphQL */ `
  query GetActiveNode {
    activeNode {
      ${NODE_FIELDS}
    }
  }
`

export const ACTIVATE_NODE = /* GraphQL */ `
  mutation ActivateNode($nodeId: ID!) {
    activateNode(nodeId: $nodeId) {
      ok
      activeNodeId
    }
  }
`

export const GET_SCENARIOS = /* GraphQL */ `
  query GetScenarios {
    scenarios {
      id
      name
      node_id
      active
      color
    }
    activeScenarioId
  }
`

export const GET_SYSTEM_SNAPSHOT = /* GraphQL */ `
  query GetSystemSnapshot {
    snapshot {
      nodes {
        ${NODE_FIELDS}
      }
      activeNodeId
    }
  }
`

// ── Thin GraphQL fetch helper ─────────────────────────────────────────────────

const API_BASE = (import.meta.env.VITE_API_BASE as string | undefined) ?? ''

export async function gqlFetch<T>(
  query: string,
  variables?: Record<string, unknown>,
): Promise<T> {
  const token = localStorage.getItem('ozma_token') ?? ''
  const res = await fetch(`${API_BASE}/graphql`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify({ query, variables }),
  })

  if (!res.ok) {
    throw new Error(`GraphQL request failed: ${res.status} ${res.statusText}`)
  }

  const json = (await res.json()) as {
    data?: T
    errors?: Array<{ message: string }>
  }

  if (json.errors?.length) {
    throw new Error(json.errors.map((e) => e.message).join('; '))
  }

  if (json.data === undefined) {
    throw new Error('GraphQL response contained no data')
  }

  return json.data
}
import type { NodeInfo } from '../types/node'
import type { Scenario } from '../store/ozmaStore'

// ── Response types ────────────────────────────────────────────────────────────

export interface GetNodesData {
  nodes: NodeInfo[]
  activeNodeId: string | null
}

export interface GetScenariosData {
  scenarios: Scenario[]
  activeScenarioId: string | null
}

export interface GetNodeByIdData {
  node: NodeInfo | null
}

export interface GetActiveNodeData {
  activeNode: NodeInfo | null
}

export interface ActivateNodeData {
  activateNode: { ok: boolean; activeNodeId: string }
}

export interface GetSystemSnapshotData {
  snapshot: { nodes: NodeInfo[]; activeNodeId: string | null }
}

// ── Shared node field list ────────────────────────────────────────────────────

const NODE_FIELDS = /* GraphQL */ `
  id
  name
  hostname
  host
  port
  role
  hw
  fw_version
  proto_version
  capabilities
  machine_class
  last_seen
  status
  active
  uptime_seconds
  ip_address
  mac_address
  platform
  version
  vnc_host
  vnc_port
  stream_port
  stream_path
  audio_type
  audio_sink
  audio_vban_port
  mic_vban_port
  capture_device
  camera_streams {
    name
    rtsp_inbound
    backchannel
    hls
  }
  frigate_host
  frigate_port
  owner_user_id
  owner_id
  shared_with
  share_permissions
  parent_node_id
  sunshine_port
  seat_count
  seat_config
  display_outputs {
    index
    source_type
    capture_source_id
    width
    height
  }
  scenario {
    id
    name
    color
  }
  hid_stats {
    total_keys
    total_clicks
    total_scrolls
    last_activity
  }
`

// ── Query documents ───────────────────────────────────────────────────────────

export const GET_NODES = /* GraphQL */ `
  query GetNodes {
    nodes {
      ${NODE_FIELDS}
    }
    activeNodeId
  }
`

export const GET_NODE_BY_ID = /* GraphQL */ `
  query GetNodeById($id: ID!) {
    node(id: $id) {
      ${NODE_FIELDS}
    }
  }
`

export const GET_ACTIVE_NODE = /* GraphQL */ `
  query GetActiveNode {
    activeNode {
      ${NODE_FIELDS}
    }
  }
`

export const ACTIVATE_NODE = /* GraphQL */ `
  mutation ActivateNode($nodeId: ID!) {
    activateNode(nodeId: $nodeId) {
      ok
      activeNodeId
    }
  }
`

export const GET_SCENARIOS = /* GraphQL */ `
  query GetScenarios {
    scenarios {
      id
      name
      node_id
      active
      color
    }
    activeScenarioId
  }
`

export const GET_SYSTEM_SNAPSHOT = /* GraphQL */ `
  query GetSystemSnapshot {
    snapshot {
      nodes {
        ${NODE_FIELDS}
      }
      activeNodeId
    }
  }
`

// ── Thin GraphQL fetch helper ─────────────────────────────────────────────────

const API_BASE = (import.meta.env.VITE_API_BASE as string | undefined) ?? ''

export async function gqlFetch<T>(
  query: string,
  variables?: Record<string, unknown>,
): Promise<T> {
  const token = localStorage.getItem('ozma_token') ?? ''
  const res = await fetch(`${API_BASE}/graphql`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify({ query, variables }),
  })

  if (!res.ok) {
    throw new Error(`GraphQL request failed: ${res.status} ${res.statusText}`)
  }

  const json = (await res.json()) as {
    data?: T
    errors?: Array<{ message: string }>
  }

  if (json.errors?.length) {
    throw new Error(json.errors.map((e) => e.message).join('; '))
  }

  if (json.data === undefined) {
    throw new Error('GraphQL response contained no data')
  }

  return json.data
}
