/**
 * ozmaStore — central Zustand store for real-time controller state.
 *
 * Populated by:
 *   • GraphQL queries / REST calls (initial load)
 *   • WebSocket events (live updates)
 *
 * Install zustand:  npm install zustand
 */
import { create } from 'zustand'
import type { NodeInfo } from '../types/node'

// ── Scenario type ─────────────────────────────────────────────────────────────

export interface Scenario {
  id: string
  name: string
  node_id: string | null
  active: boolean
  color?: string
}

// ── Store shape ───────────────────────────────────────────────────────────────

interface OzmaState {
  // Data
  nodes: NodeInfo[]
  activeNodeId: string | null
  scenarios: Scenario[]
  wsConnected: boolean

  // Setters (used by data loaders)
  setNodes: (nodes: NodeInfo[]) => void
  setActiveNodeId: (id: string | null) => void
  setScenarios: (scenarios: Scenario[]) => void
  setWsConnected: (connected: boolean) => void

  // WebSocket event dispatcher
  handleWsEvent: (event: Record<string, unknown>) => void
}

// ── Store implementation ──────────────────────────────────────────────────────

export const useOzmaStore = create<OzmaState>((set) => ({
  nodes: [],
  activeNodeId: null,
  scenarios: [],
  wsConnected: false,

  setNodes: (nodes) => set({ nodes }),
  setActiveNodeId: (id) => set({ activeNodeId: id }),
  setScenarios: (scenarios) => set({ scenarios }),
  setWsConnected: (wsConnected) => set({ wsConnected }),

  handleWsEvent: (event) => {
    const type = event.type as string | undefined

    switch (type) {
      // ── Initial snapshot sent on WS connect ──────────────────────────────
      case 'snapshot': {
        const data = event.data as Record<string, unknown> | undefined
        if (!data) break
        if (Array.isArray(data.nodes)) {
          // Mark each node's active flag from active_node_id
          const activeId = (data.active_node_id as string | null) ?? null
          const nodes = (data.nodes as NodeInfo[]).map((n) => ({
            ...n,
            active: n.id === activeId,
            status: 'online' as const,
          }))
          set({ nodes, activeNodeId: activeId })
        }
        break
      }

      // ── Node lifecycle ────────────────────────────────────────────────────
      case 'node.online':
      case 'node_discovered': {
        const node = (event.node ?? event.data) as NodeInfo | undefined
        if (!node) break
        const incoming: NodeInfo = { ...node, status: 'online' }
        set((s) => {
          const exists = s.nodes.some((n) => n.id === incoming.id)
          return {
            nodes: exists
              ? s.nodes.map((n) => (n.id === incoming.id ? { ...n, ...incoming } : n))
              : [...s.nodes, incoming],
          }
        })
        break
      }

      case 'node.offline':
      case 'node_lost': {
        const nodeId =
          (event.node_id as string | undefined) ??
          (event.node as NodeInfo | undefined)?.id
        if (!nodeId) break
        set((s) => ({
          nodes: s.nodes.map((n) => (n.id === nodeId ? { ...n, status: 'offline' as const } : n)),
        }))
        break
      }

      case 'node.updated': {
        const node = event.node as (Partial<NodeInfo> & { id: string }) | undefined
        if (!node) break
        set((s) => ({
          nodes: s.nodes.map((n) => (n.id === node.id ? { ...n, ...node } : n)),
        }))
        break
      }

      case 'node.switched': {
        const nodeId = event.node_id as string | undefined
        if (nodeId === undefined) break
        set((s) => ({
          activeNodeId: nodeId,
          nodes: s.nodes.map((n) => ({ ...n, active: n.id === nodeId })),
        }))
        break
      }

      // ── Scenario lifecycle ────────────────────────────────────────────────
      case 'scenario.activated':
      case 'scenario_switched': {
        const scenarioId =
          (event.scenario_id as string | undefined) ??
          (event.scenario as Scenario | undefined)?.id
        if (!scenarioId) break
        set((s) => ({
          scenarios: s.scenarios.map((sc) => ({
            ...sc,
            active: sc.id === scenarioId,
          })),
        }))
        break
      }

      case 'scenario.created': {
        const scenario = event.scenario as Scenario | undefined
        if (!scenario) break
        set((s) => ({ scenarios: [...s.scenarios, scenario] }))
        break
      }

      case 'scenario.deleted': {
        const scenarioId = event.scenario_id as string | undefined
        if (!scenarioId) break
        set((s) => ({
          scenarios: s.scenarios.filter((sc) => sc.id !== scenarioId),
        }))
        break
      }

      default:
        // Unknown event types are silently ignored — forward compatibility.
        break
    }
  },
}))
/**
 * ozmaStore — central Zustand store for real-time controller state.
 *
 * Populated by:
 *   • GraphQL queries / REST calls (initial load)
 *   • WebSocket events (live updates)
 *
 * Install zustand:  npm install zustand
 */
import { create } from 'zustand'
import type { NodeInfo } from '../types/node'

// ── Scenario type ─────────────────────────────────────────────────────────────

export interface Scenario {
  id: string
  name: string
  node_id: string | null
  active: boolean
  color?: string
}

// ── Store shape ───────────────────────────────────────────────────────────────

interface OzmaState {
  // Data
  nodes: NodeInfo[]
  activeNodeId: string | null
  scenarios: Scenario[]
  wsConnected: boolean

  // Setters (used by data loaders)
  setNodes: (nodes: NodeInfo[]) => void
  setActiveNodeId: (id: string | null) => void
  setScenarios: (scenarios: Scenario[]) => void
  setWsConnected: (connected: boolean) => void

  // WebSocket event dispatcher
  handleWsEvent: (event: Record<string, unknown>) => void
}

// ── Store implementation ──────────────────────────────────────────────────────

export const useOzmaStore = create<OzmaState>((set) => ({
  nodes: [],
  activeNodeId: null,
  scenarios: [],
  wsConnected: false,

  setNodes: (nodes) => set({ nodes }),
  setActiveNodeId: (id) => set({ activeNodeId: id }),
  setScenarios: (scenarios) => set({ scenarios }),
  setWsConnected: (wsConnected) => set({ wsConnected }),

  handleWsEvent: (event) => {
    const type = event.type as string | undefined

    switch (type) {
      // ── Initial snapshot sent on WS connect ──────────────────────────────
      case 'snapshot': {
        const data = event.data as Record<string, unknown> | undefined
        if (!data) break
        if (Array.isArray(data.nodes)) {
          // Mark each node's active flag from active_node_id
          const activeId = (data.active_node_id as string | null) ?? null
          const nodes = (data.nodes as NodeInfo[]).map((n) => ({
            ...n,
            active: n.id === activeId,
            status: 'online' as const,
          }))
          set({ nodes, activeNodeId: activeId })
        }
        break
      }

      // ── Node lifecycle ────────────────────────────────────────────────────
      case 'node.online':
      case 'node_discovered': {
        const node = (event.node ?? event.data) as NodeInfo | undefined
        if (!node) break
        const incoming: NodeInfo = { ...node, status: 'online' }
        set((s) => {
          const exists = s.nodes.some((n) => n.id === incoming.id)
          return {
            nodes: exists
              ? s.nodes.map((n) => (n.id === incoming.id ? { ...n, ...incoming } : n))
              : [...s.nodes, incoming],
          }
        })
        break
      }

      case 'node.offline':
      case 'node_lost': {
        const nodeId =
          (event.node_id as string | undefined) ??
          (event.node as NodeInfo | undefined)?.id
        if (!nodeId) break
        set((s) => ({
          nodes: s.nodes.map((n) => (n.id === nodeId ? { ...n, status: 'offline' as const } : n)),
        }))
        break
      }

      case 'node.updated': {
        const node = event.node as (Partial<NodeInfo> & { id: string }) | undefined
        if (!node) break
        set((s) => ({
          nodes: s.nodes.map((n) => (n.id === node.id ? { ...n, ...node } : n)),
        }))
        break
      }

      case 'node.switched': {
        const nodeId = event.node_id as string | undefined
        if (nodeId === undefined) break
        set((s) => ({
          activeNodeId: nodeId,
          nodes: s.nodes.map((n) => ({ ...n, active: n.id === nodeId })),
        }))
        break
      }

      // ── Scenario lifecycle ────────────────────────────────────────────────
      case 'scenario.activated':
      case 'scenario_switched': {
        const scenarioId =
          (event.scenario_id as string | undefined) ??
          (event.scenario as Scenario | undefined)?.id
        if (!scenarioId) break
        set((s) => ({
          scenarios: s.scenarios.map((sc) => ({
            ...sc,
            active: sc.id === scenarioId,
          })),
        }))
        break
      }

      case 'scenario.created': {
        const scenario = event.scenario as Scenario | undefined
        if (!scenario) break
        set((s) => ({ scenarios: [...s.scenarios, scenario] }))
        break
      }

      case 'scenario.deleted': {
        const scenarioId = event.scenario_id as string | undefined
        if (!scenarioId) break
        set((s) => ({
          scenarios: s.scenarios.filter((sc) => sc.id !== scenarioId),
        }))
        break
      }

      default:
        // Unknown event types are silently ignored — forward compatibility.
        break
    }
  },
}))
