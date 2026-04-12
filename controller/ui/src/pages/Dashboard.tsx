/**
 * Dashboard — main overview page.
 *
 * • Fetches nodes + scenarios on mount (GraphQL with REST fallback).
 * • Subscribes to live updates via the WebSocket ozmaStore.
 * • Renders a NodeCard grid and the active scenario banner.
 */
import { useEffect, useState } from 'react'
import { useOzmaStore } from '../store/ozmaStore'
import { useWebSocket } from '../ws/useWebSocket'
import { gqlFetch, GET_NODES, GET_SCENARIOS } from '../graphql/queries'
import type { GetNodesData, GetScenariosData } from '../graphql/queries'
import type { NodeInfo } from '../types/node'
import type { Scenario } from '../store/ozmaStore'
import NodeCard from '../components/NodeCard'

const API_BASE = (import.meta.env.VITE_API_BASE as string | undefined) ?? ''

async function fetchNodesRest(): Promise<GetNodesData> {
  const token = localStorage.getItem('ozma_token') ?? ''
  const res = await fetch(`${API_BASE}/api/v1/nodes`, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  })
  if (!res.ok) throw new Error(`/api/v1/nodes ${res.status}`)
  const data = (await res.json()) as { nodes: NodeInfo[]; active_node_id: string | null }
  return { nodes: data.nodes, activeNodeId: data.active_node_id }
}

async function fetchScenariosRest(): Promise<GetScenariosData> {
  const token = localStorage.getItem('ozma_token') ?? ''
  const res = await fetch(`${API_BASE}/api/v1/scenarios`, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  })
  if (!res.ok) throw new Error(`/api/v1/scenarios ${res.status}`)
  const data = (await res.json()) as { scenarios: Scenario[]; active_id: string | null }
  return { scenarios: data.scenarios, activeScenarioId: data.active_id }
}

export default function Dashboard() {
  useWebSocket()

  const nodes        = useOzmaStore((s) => s.nodes)
  const activeNodeId = useOzmaStore((s) => s.activeNodeId)
  const scenarios    = useOzmaStore((s) => s.scenarios)
  const setNodes     = useOzmaStore((s) => s.setNodes)
  const setActiveNodeId = useOzmaStore((s) => s.setActiveNodeId)
  const setScenarios = useOzmaStore((s) => s.setScenarios)
  const wsConnected  = useOzmaStore((s) => s.wsConnected)

  const [loading, setLoading] = useState(true)
  const [error, setError]     = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false

    async function load() {
      try {
        setLoading(true)
        setError(null)

        let nodesData: GetNodesData
        let scenariosData: GetScenariosData

        try {
          ;[nodesData, scenariosData] = await Promise.all([
            gqlFetch<GetNodesData>(GET_NODES),
            gqlFetch<GetScenariosData>(GET_SCENARIOS),
          ])
        } catch {
          // GraphQL not yet available — fall back to REST
          ;[nodesData, scenariosData] = await Promise.all([
            fetchNodesRest(),
            fetchScenariosRest(),
          ])
        }

        if (cancelled) return

        const activeId = nodesData.activeNodeId
        setNodes(
          nodesData.nodes.map((n) => ({
            ...n,
            active: n.id === activeId,
            status: (n.status ?? 'online') as NodeInfo['status'],
          })),
        )
        setActiveNodeId(activeId)
        setScenarios(scenariosData.scenarios)
      } catch (err) {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : 'Failed to load dashboard data')
        }
      } finally {
        if (!cancelled) setLoading(false)
      }
    }

    void load()
    return () => { cancelled = true }
  }, [setNodes, setActiveNodeId, setScenarios])

  const activeScenario = scenarios.find((sc) => sc.active) ?? null

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64 text-zinc-400">
        Loading…
      </div>
    )
  }

  if (error) {
    return (
      <div className="p-4 rounded bg-red-900/30 border border-red-700 text-red-300">
        <strong>Error:</strong> {error}
      </div>
    )
  }

  return (
    <div className="space-y-6 p-6">
      {/* Connection status pill */}
      <div className="flex items-center gap-2 text-sm">
        <span
          className={`inline-block w-2 h-2 rounded-full ${
            wsConnected ? 'bg-green-400' : 'bg-yellow-400 animate-pulse'
          }`}
        />
        <span className="text-zinc-400">
          {wsConnected ? 'Live' : 'Reconnecting…'}
        </span>
      </div>

      {/* Active scenario banner */}
      {activeScenario && (
        <div className="rounded-lg bg-indigo-900/40 border border-indigo-700 px-4 py-3">
          <p className="text-xs text-indigo-400 uppercase tracking-wide mb-0.5">
            Active scenario
          </p>
          <p className="text-zinc-100 font-semibold">{activeScenario.name}</p>
        </div>
      )}

      {/* Node grid */}
      {nodes.length === 0 ? (
        <p className="text-zinc-500 text-sm">No nodes discovered yet.</p>
      ) : (
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {nodes.map((node) => (
            <NodeCard
              key={node.id}
              node={node}
              className={node.id === activeNodeId ? 'ring-2 ring-indigo-500' : ''}
            />
          ))}
        </div>
      )}
    </div>
  )
  return <div className="p-6 text-oz-text">Dashboard</div>
}
