import { create } from 'zustand'
import { Node, NodeListResponse } from '../types'

interface NodeStore {
  nodes: Node[]
  loading: boolean
  error: string | null
  fetchNodes: () => Promise<void>
  addNode: (node: Node) => void
  updateNode: (node: Node) => void
  removeNode: (id: string) => void
}

export const useNodeStore = create<NodeStore>((set) => ({
  nodes: [],
  loading: false,
  error: null,

  fetchNodes: async () => {
    set({ loading: true, error: null })
    try {
      const token = localStorage.getItem('ozma_auth_token')
      const response = await fetch('/api/v1/nodes', {
        headers: token ? { Authorization: `Bearer ${token}` } : undefined,
      })

      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.statusText}`)
      }

      const data: NodeListResponse = await response.json()
      set({ nodes: data.nodes, loading: false, error: null })
    } catch (err) {
      set({ loading: false, error: err instanceof Error ? err.message : 'Unknown error' })
    }
  },

  addNode: (node) =>
    set((state) => ({
      nodes: [...state.nodes, node],
    })),

  updateNode: (node) =>
    set((state) => ({
      nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
    })),

  removeNode: (id) =>
    set((state) => ({
      nodes: state.nodes.filter((n) => n.id !== id),
    })),
}))
