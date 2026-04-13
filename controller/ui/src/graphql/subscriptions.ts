/**
 * GraphQL subscription documents and types.
 *
 * The controller exposes subscriptions over the WebSocket transport at
 * /graphql (graphql-ws protocol).  For most real-time use-cases the
 * simpler /api/v1/events WebSocket (handled by useWebSocket + ozmaStore)
 * is preferred.  These subscriptions are provided for consumers that
 * prefer the GraphQL subscription model (e.g. Apollo Client / urql).
 *
 * Install graphql-ws:  npm install graphql-ws
 */
import type { NodeInfo } from '../types/node'
import type { Scenario } from '../store/ozmaStore'

// ── NodeStatusChanged ─────────────────────────────────────────────────────────

export const NODE_STATUS_CHANGED = /* GraphQL */ `
  subscription NodeStatusChanged {
    nodeStatusChanged {
      id
      name
      hostname
      host
      port
      role
      hw
      fw_version
      machine_class
      status
      active
      seat_count
      capabilities
      display_outputs {
        index
        width
        height
      }
    }
  }
`

export interface NodeStatusChangedData {
  nodeStatusChanged: NodeInfo
}

// ── ScenarioActivated ─────────────────────────────────────────────────────────

export const SCENARIO_ACTIVATED = /* GraphQL */ `
  subscription ScenarioActivated {
    scenarioActivated {
      id
      name
      node_id
      active
      color
    }
  }
`

export interface ScenarioActivatedData {
  scenarioActivated: Scenario
}

// ── graphql-ws client factory ─────────────────────────────────────────────────
//
// Usage:
//   import { createGqlWsClient } from './subscriptions'
//   const client = createGqlWsClient()
//   const unsub = client.subscribe(
//     { query: NODE_STATUS_CHANGED },
//     { next: ({ data }) => console.log(data?.nodeStatusChanged) },
//   )

export function createGqlWsClient() {
  // Lazy require so the module is tree-shaken when graphql-ws is absent.
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const { createClient } = require('graphql-ws') as typeof import('graphql-ws')

  const apiBase = (import.meta.env.VITE_API_BASE as string | undefined) ?? ''
  const wsBase =
    apiBase.replace(/^http/, 'ws') ||
    `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}`

  const token = localStorage.getItem('ozma_token') ?? ''

  return createClient({
    url: `${wsBase}/graphql`,
    connectionParams: token ? { Authorization: `Bearer ${token}` } : {},
  })
}
/**
 * GraphQL subscription documents and types.
 *
 * The controller exposes subscriptions over the WebSocket transport at
 * /graphql (graphql-ws protocol).  For most real-time use-cases the
 * simpler /api/v1/events WebSocket (handled by useWebSocket + ozmaStore)
 * is preferred.  These subscriptions are provided for consumers that
 * prefer the GraphQL subscription model (e.g. Apollo Client / urql).
 *
 * Install graphql-ws:  npm install graphql-ws
 */
import type { NodeInfo } from '../types/node'
import type { Scenario } from '../store/ozmaStore'

// ── NodeStatusChanged ─────────────────────────────────────────────────────────

export const NODE_STATUS_CHANGED = /* GraphQL */ `
  subscription NodeStatusChanged {
    nodeStatusChanged {
      id
      name
      hostname
      host
      port
      role
      hw
      fw_version
      machine_class
      status
      active
      seat_count
      capabilities
      display_outputs {
        index
        width
        height
      }
    }
  }
`

export interface NodeStatusChangedData {
  nodeStatusChanged: NodeInfo
}

// ── ScenarioActivated ─────────────────────────────────────────────────────────

export const SCENARIO_ACTIVATED = /* GraphQL */ `
  subscription ScenarioActivated {
    scenarioActivated {
      id
      name
      node_id
      active
      color
    }
  }
`

export interface ScenarioActivatedData {
  scenarioActivated: Scenario
}

// ── graphql-ws client factory ─────────────────────────────────────────────────
//
// Usage:
//   import { createGqlWsClient } from './subscriptions'
//   const client = createGqlWsClient()
//   const unsub = client.subscribe(
//     { query: NODE_STATUS_CHANGED },
//     { next: ({ data }) => console.log(data?.nodeStatusChanged) },
//   )

export function createGqlWsClient() {
  // Lazy require so the module is tree-shaken when graphql-ws is absent.
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const { createClient } = require('graphql-ws') as typeof import('graphql-ws')

  const apiBase = (import.meta.env.VITE_API_BASE as string | undefined) ?? ''
  const wsBase =
    apiBase.replace(/^http/, 'ws') ||
    `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}`

  const token = localStorage.getItem('ozma_token') ?? ''

  return createClient({
    url: `${wsBase}/graphql`,
    connectionParams: token ? { Authorization: `Bearer ${token}` } : {},
  })
}
