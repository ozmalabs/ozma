import { create } from 'zustand'
import { useEffect } from 'react'
import { client } from '../graphql/client'
import { 
  GET_NODES, 
  GET_ACTIVE_NODE, 
  GET_NODE_BY_ID,
  SUBSCRIBE_NODE_CHANGED,
  SUBSCRIBE_NODE_ADDED,
  SUBSCRIBE_NODE_REMOVED,
  ACTIVATE_NODE
} from '../graphql/queries'

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
  node_id?: string
  transition_in?: {
    style: string
    duration_ms: number
  }
  motion?: {
    device_id: string
    axis: string
    position: number
  }[]
  bluetooth?: {
    connect: string[]
    disconnect: string[]
  }[]
  capture_source?: string
  capture_sources?: string[]
  wallpaper?: {
    mode: string
    color?: string
    image?: string
    url?: string
  }
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
  api_port?: number | null
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
  seat_count?: number
  seat_config?: string
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
  setWsConnected: (connected: boolean) => void
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

  setWsConnected: (connected: boolean) => {
    set({ wsConnected: connected })
  },

  fetchNodes: async () => {
    try {
      set({ loading: true, error: null })
      const result = await client.query(GET_NODES, {}).toPromise()
      if (result.data?.nodes) {
        const nodes = result.data.nodes as NodeInfo[]
        const activeNode = getActiveNodeFromList(nodes, null)
        set({ nodes, activeNode, loading: false, error: null })
      } else {
        set({ loading: false, error: 'Failed to load nodes' })
      }
    } catch (error) {
      console.error('Error fetching nodes:', error)
      set({ loading: false, error: 'Failed to load nodes' })
    }
  },

  fetchNodeById: async (id: string) => {
    try {
      set({ loading: true, error: null })
      const result = await client.query(GET_NODE_BY_ID, { id }).toPromise()
      if (result.data?.node) {
        const node = result.data.node as NodeInfo
        set({ selectedNode: node, loading: false, error: null })
      } else {
        set({ loading: false, error: 'Node not found' })
      }
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
    const subscriptions: (() => void)[] = []

    // Subscribe to node changes
    const nodeChangedSub = client.subscription(SUBSCRIBE_NODE_CHANGED, {}).subscribe({
      next: (result) => {
        if (result.data?.node_changed) {
          get().updateNode(result.data.node_changed as NodeInfo)
        }
      },
      error: (error) => {
        console.error('Node changed subscription error:', error)
      },
    })
    subscriptions.push(() => nodeChangedSub.unsubscribe())

    // Subscribe to node added events
    const nodeAddedSub = client.subscription(SUBSCRIBE_NODE_ADDED, {}).subscribe({
      next: (result) => {
        if (result.data?.node_added) {
          get().addNode(result.data.node_added as NodeInfo)
        }
      },
      error: (error) => {
        console.error('Node added subscription error:', error)
      },
    })
    subscriptions.push(() => nodeAddedSub.unsubscribe())

    // Subscribe to node removed events
    const nodeRemovedSub = client.subscription(SUBSCRIBE_NODE_REMOVED, {}).subscribe({
      next: (result) => {
        if (result.data?.node_removed) {
          get().removeNode(result.data.node_removed.id)
        }
      },
      error: (error) => {
        console.error('Node removed subscription error:', error)
      },
    })
    subscriptions.push(() => nodeRemovedSub.unsubscribe())

    // Also keep REST event WebSocket for compatibility
    try {
      const wsUrl = typeof window !== 'undefined' && window.location.protocol === 'https:'
        ? `wss://${window.location.host}/api/v1/events`
        : `ws://localhost:7380/api/v1/events`
      const token = localStorage.getItem('ozma_token')

      const socket = new WebSocket(`${wsUrl}${token ? `?token=${encodeURIComponent(token)}` : ''}`)

      socket.onopen = () => {
        console.log('REST WebSocket connected')
        get().setWsConnected(true)
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
        console.log('REST WebSocket disconnected')
        get().setWsConnected(false)
      }

      socket.onerror = (error) => {
        console.error('REST WebSocket error:', error)
      }
    } catch (error) {
      console.error('Failed to connect to REST WebSocket:', error)
    }

    return () => {
      subscriptions.forEach((unsubscribe) => unsubscribe())
    }
  },

  activateNode: async (nodeId: string) => {
    try {
      const result = await client.mutation(ACTIVATE_NODE, { nodeId }).toPromise()
      if (result.data?.activate_node) {
        // Refresh node list after activation
        await get().fetchNodes()
      } else {
        throw new Error('Failed to activate node')
      }
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

  removeNode: (id: string) => {
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

function handleWebSocketMessage(data: unknown) {
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
    const activeNode = getActiveNodeFromList(nodesList, eventData.active_node_id)
    useNodesStore.getState().nodes.forEach((n) => useNodesStore.getState().removeNode(n.id))
    nodesList.forEach((n) => useNodesStore.getState().addNode(n))
    useNodesStore.getState().setActiveNode(activeNode)
    return
  }

  // Handle node online event
  if (type === 'node.online' && node) {
    useNodesStore.getState().addNode(node)
    return
  }

  // Handle node offline event
  if (type === 'node.offline' && node_id) {
    useNodesStore.getState().removeNode(node_id)
    return
  }

  // Handle node switched event
  if (type === 'node.switched' && node_id) {
    const activeNode = useNodesStore.getState().nodes.find((n) => n.id === node_id)
    useNodesStore.getState().setActiveNode(activeNode || null)
    return
  }

  // Handle general node updates
  if (type === 'node_updated' && node) {
    useNodesStore.getState().updateNode(node)
    return
  }
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
