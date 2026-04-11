import { create } from 'zustand'
import { useEffect } from 'react'
import { client } from '../graphql/client'
import { GET_NODES, GET_ACTIVE_NODE, SUBSCRIBE_NODE_STATE } from '../graphql/queries'

export interface CameraStream {
  name: string
  rtsp_inbound: string
  backchannel: string
  hls: string
}

export interface DisplayOutput {
  index: number
  source_type: string
  capture_source_id: string
  width: number
  height: number
}

export interface Scenario {
  id: string
  name: string
  color: string
}

export interface HIDStats {
  total_keys: number
  total_clicks: number
  total_scrolls: number
  last_activity: string
}

export interface NodeInfo {
  id: string
  name: string | null
  host: string
  port: number
  role: string
  hw: string
  fw_version: string
  proto_version: number
  capabilities: string[]
  machine_class: 'workstation' | 'server' | 'kiosk' | 'camera'
  last_seen: string
  status?: 'online' | 'offline' | 'connecting'
  display_outputs: DisplayOutput[]
  vnc_host: string | null
  vnc_port: number | null
  stream_port: number | null
  stream_path: string | null
  api_port: number | null
  audio_type: string | null
  audio_sink: string | null
  audio_vban_port: number | null
  mic_vban_port: number | null
  capture_device: string | null
  camera_streams: CameraStream[]
  frigate_host: string | null
  frigate_port: number | null
  owner_user_id: string
  owner_id: string
  shared_with: string[]
  share_permissions: Record<string, string>
  parent_node_id: string
  sunshine_port: number | null
  uptime_seconds?: number
  ip_address?: string
  mac_address?: string
  hostname?: string
  platform?: string
  version?: string
  scenario?: Scenario
  hid_stats?: HIDStats
  active?: boolean
}

interface NodesStore {
  nodes: NodeInfo[]
  activeNode: NodeInfo | null
  loading: boolean
  error: string | null
  wsConnected: boolean
  selectedNodeId: string | null
  selectedNode: NodeInfo | null
  fetchNodes: () => Promise<void>
  fetchNodeById: (id: string) => Promise<void>
  selectNode: (id: string | null) => void
  connectWebSocket: () => () => void
  activateNode: (nodeId: string) => Promise<void>
  addNode: (node: NodeInfo) => void
  updateNode: (node: NodeInfo) => void
  removeNode: (id: string) => void
  setActiveNode: (node: NodeInfo | null) => void
  updateSelectedNode: (node: NodeInfo) => void
}

const API_BASE = '/api/v1'

export const useNodesStore = create<NodesStore>((set, get) => ({
  nodes: [],
  activeNode: null,
  loading: true,
  error: null,
  wsConnected: false,
  selectedNodeId: null,
  selectedNode: null,

  fetchNodes: async () => {
    try {
      set({ loading: true, error: null })
      const response = await fetch(`${API_BASE}/nodes`)
      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.statusText}`)
      }
      const data = await response.json()
      set({ nodes: data.nodes || data, loading: false, error: null })
    } catch (error) {
      console.error('Error fetching nodes:', error)
      set({ loading: false, error: 'Failed to load nodes' })
    }
  },

  fetchNodeById: async (id: string) => {
    try {
      set({ loading: true, error: null })
      const response = await fetch(`${API_BASE}/nodes/${id}`)
      if (!response.ok) {
        throw new Error(`Failed to fetch node: ${response.statusText}`)
      }
      const data = await response.json()
      set({ selectedNode: data, loading: false, error: null })
    } catch (error) {
      console.error('Error fetching node:', error)
      set({ loading: false, error: 'Failed to load node details' })
    }
  },

  selectNode: (id: string | null) => {
    set({ selectedNodeId: id, selectedNode: null })
    if (id) {
      get().fetchNodeById(id)
    }
  },

  connectWebSocket: () => {
    // Try GraphQL WebSocket first, fall back to REST events
    const wsUrl = 'ws://localhost:7380/api/v1/events'
    const token = localStorage.getItem('ozma_token')
    
    const socket = new WebSocket(`${wsUrl}${token ? `?token=${encodeURIComponent(token)}` : ''}`)

    socket.onopen = () => {
      console.log('WebSocket connected')
      set({ wsConnected: true })
    }

    socket.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        handleWebSocketMessage(data)
      } catch (error) {
        console.error('Error parsing WebSocket message:', error)
      }
    }

    socket.onclose = () => {
      console.log('WebSocket disconnected')
      set({ wsConnected: false })
    }

    socket.onerror = (error) => {
      console.error('WebSocket error:', error)
    }

    const handleWebSocketMessage = (data: unknown) => {
      if (!data || typeof data !== 'object') return

      const { type, node, node_id, data: eventData } = data as {
        type?: string
        node?: NodeInfo
        node_id?: string
        data?: { nodes?: NodeInfo[]; active_node_id?: string }
      }

      // Handle snapshot event on connect
      if (type === 'snapshot' && eventData) {
        const nodesList = eventData.nodes ? Object.values(eventData.nodes) : []
        set({ nodes: nodesList, activeNode: getActiveNodeFromList(nodesList, eventData.active_node_id) })
        return
      }

      // Handle node online event
      if (type === 'node.online' && node) {
        set((state) => {
          const exists = state.nodes.find((n) => n.id === node.id)
          if (exists) return state
          return { nodes: [...state.nodes, node] }
        })
        return
      }

      // Handle node offline event
      if (type === 'node.offline' && node_id) {
        set((state) => ({
          nodes: state.nodes.filter((n) => n.id !== node_id),
          activeNode: state.activeNode?.id === node_id ? null : state.activeNode,
          selectedNode: state.selectedNode?.id === node_id ? null : state.selectedNode,
        }))
        return
      }

      // Handle node switched event
      if (type === 'node.switched' && node_id) {
        const activeNode = get().nodes.find((n) => n.id === node_id)
        set({ activeNode })
        return
      }

      // Handle general node updates
      if (type === 'node_updated' && node) {
        get().updateNode(node)
        return
      }
    }

    return () => {
      socket.close()
    }
  },

  activateNode: async (nodeId: string) => {
    try {
      const response = await fetch(`${API_BASE}/nodes/${nodeId}/activate`, {
        method: 'POST',
        headers: {
          'Authorization': `Bearer ${localStorage.getItem('ozma_token') || ''}`,
        },
      })
      if (!response.ok) {
        throw new Error(`Failed to activate node: ${response.statusText}`)
      }
      // Refresh node list after activation
      await get().fetchNodes()
    } catch (error) {
      console.error('Error activating node:', error)
      throw error
    }
  },

  addNode: (node) => {
    set((state) => {
      const exists = state.nodes.find((n) => n.id === node.id)
      if (exists) return state
      return { nodes: [...state.nodes, node] }
    })
  },

  updateNode: (node) => {
    set((state) => ({
      nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
      selectedNode: state.selectedNode?.id === node.id ? node : state.selectedNode,
      activeNode: state.activeNode?.id === node.id ? node : state.activeNode,
    }))
  },

  removeNode: (id) => {
    set((state) => ({
      nodes: state.nodes.filter((n) => n.id !== id),
      activeNode: state.activeNode?.id === id ? null : state.activeNode,
      selectedNode: state.selectedNode?.id === id ? null : state.selectedNode,
    }))
  },

  setActiveNode: (node) => {
    set({ activeNode: node })
  },

  updateSelectedNode: (node) => {
    set((state) => ({
      selectedNode: state.selectedNode?.id === node.id ? node : state.selectedNode,
    }))
  },
}))

function getActiveNodeFromList(nodes: NodeInfo[], activeId: string | null): NodeInfo | null {
  if (!activeId) return null
  return nodes.find((n) => n.id === activeId) || null
}

// Custom hook for using nodes with auto-fetch and WebSocket
export function useNodes() {
  const { nodes, loading, error, fetchNodes, connectWebSocket, wsConnected } = useNodesStore()

  useEffect(() => {
    fetchNodes()
    const disconnect = connectWebSocket()
    return disconnect
  }, [fetchNodes, connectWebSocket])

  return { nodes, loading, error, wsConnected }
}

// Custom hook for selected node detail
export function useSelectedNode() {
  const { selectedNode, selectedNodeId, selectNode, fetchNodeById, updateSelectedNode } = useNodesStore()

  return {
    selectedNode,
    selectedNodeId,
    selectNode,
    fetchNodeById,
    updateSelectedNode,
  }
}

// Custom hook for active node
export function useActiveNode() {
  const { activeNode } = useNodesStore()

  return { activeNode }
}

// Custom hook for node activation
export function useNodeActivation() {
  const { activateNode } = useNodesStore()

  return { activateNode }
}
