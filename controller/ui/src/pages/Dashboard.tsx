import { useState, useEffect, useCallback, useMemo } from 'react'
import { motion, Reorder } from 'framer-motion'
import { StatusDot } from '../components/StatusDot'
import { useNodesStore } from '../store/useNodesStore'
import { useNodesQuery, useNodeUpdates } from '../hooks/useGraphQL'
import { NodeInfo } from '../types/node'

// localStorage key for node order
const NODE_ORDER_KEY = 'ozma_node_order'

// Load saved node order from localStorage
function loadNodeOrder(): string[] {
  try {
    const saved = localStorage.getItem(NODE_ORDER_KEY)
    if (saved) {
      return JSON.parse(saved)
    }
  } catch (error) {
    console.error('Failed to load node order from localStorage:', error)
  }
  return []
}

// Save node order to localStorage
function saveNodeOrder(order: string[]) {
  try {
    localStorage.setItem(NODE_ORDER_KEY, JSON.stringify(order))
  } catch (error) {
    console.error('Failed to save node order to localStorage:', error)
  }
}

export default function Dashboard() {
  const { nodes, loading: storeLoading, error: storeError } = useNodesStore()
  const { nodes: graphqlNodes, error: graphqlError } = useNodeUpdates()

  // Track active node (for quick-switch)
  const [activeNodeId, setActiveNodeId] = useState<string | null>(null)

  // Track local node order from localStorage
  const [nodeOrder, setNodeOrder] = useState<string[]>(loadNodeOrder())

  // Get nodes from either store or GraphQL, prefer GraphQL for real-time data
  const allNodes = useMemo(() => {
    // Use GraphQL nodes if available, otherwise fall back to store nodes
    return graphqlNodes.length > 0 ? graphqlNodes : nodes
  }, [graphqlNodes, nodes])

  // Handle node switch
  const handleNodeSwitch = useCallback((node: NodeInfo) => {
    console.log('Switching to node:', node.id, node.name)
    setActiveNodeId(node.id)

    // Update node in store
    const { updateNode } = useNodesStore.getState()
    updateNode({ ...node, active: true })
  }, [])

  // Handle drag and drop reordering
  const handleDragEnd = useCallback((oldIndex: number, newIndex: number) => {
    if (newIndex >= allNodes.length || oldIndex >= allNodes.length) return

    const newNodes = [...allNodes]
    const [removed] = newNodes.splice(oldIndex, 1)
    newNodes.splice(newIndex, 0, removed)

    // Update order array
    const newOrder = newNodes.map(n => n.id)
    setNodeOrder(newOrder)
    saveNodeOrder(newOrder)
  }, [allNodes])

  // Sort nodes based on localStorage order
  const sortedNodes = useMemo(() => {
    if (nodeOrder.length === 0) return allNodes

    const nodeMap = new Map(allNodes.map(n => [n.id, n]))
    const sorted: NodeInfo[] = []
    const remaining: NodeInfo[] = []

    nodeOrder.forEach(id => {
      const node = nodeMap.get(id)
      if (node) sorted.push(node)
    })

    // Add any nodes not in order list
    allNodes.forEach(node => {
      if (!nodeOrder.includes(node.id)) {
        remaining.push(node)
      }
    })

    return [...sorted, ...remaining]
  }, [allNodes, nodeOrder])

  // Determine loading state
  const loading = storeLoading && allNodes.length === 0

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
          <p className="text-muted-foreground">Loading nodes...</p>
        </div>
      </div>
    )
  }

  if (storeError || graphqlError) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center max-w-md">
          <div className="text-destructive mb-4">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="48"
              height="48"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <circle cx="12" cy="12" r="10" />
              <line x1="15" x2="9" y1="9" y2="15" />
              <line x1="9" x2="15" y1="9" y2="15" />
            </svg>
          </div>
          <h3 className="text-xl font-semibold mb-2">Failed to load nodes</h3>
          <p className="text-muted-foreground mb-6">{storeError?.message || graphqlError?.message}</p>
          <button
            onClick={() => window.location.reload()}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="h-full overflow-y-auto">
      {/* Header */}
      <div className="mb-6">
        <div className="flex items-center justify-between">
          <div>
            <h1 className="text-3xl font-bold text-foreground">Dashboard</h1>
            <p className="text-muted-foreground mt-1">
              {sortedNodes.length} node{sortedNodes.length !== 1 ? 's' : ''} connected
            </p>
          </div>

          {/* Quick status indicator */}
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2 px-4 py-2 bg-card rounded-lg border border-border">
              <div className="relative">
                <div className="h-2.5 w-2.5 rounded-full bg-emerald-500 animate-pulse" />
                <div className="absolute inset-0 h-2.5 w-2.5 rounded-full bg-emerald-500 animate-ping opacity-50" />
              </div>
              <div className="text-sm">
                <div className="font-medium text-foreground">System Online</div>
                <div className="text-xs text-muted-foreground">All systems nominal</div>
              </div>
            </div>

            <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors flex items-center gap-2">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="16"
                height="16"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M5 12h14" />
                <path d="M12 5v14" />
              </svg>
              Add Node
            </button>
          </div>
        </div>
      </div>

      {/* Node Grid */}
      <div className="bg-background rounded-xl border border-border p-4">
        <Reorder.Group
          axis="xy"
          values={sortedNodes}
          onReorder={(newOrder: NodeInfo[]) => {
            // Map the new order back to indices for our drag handler
            const oldIndices = newOrder.map(node => sortedNodes.indexOf(node))
            const newIndices = newOrder.map(node => nodeOrder.indexOf(node.id))
            // Find the moved node
            const movedNode = newOrder.find((node, i) => oldIndices[i] !== newIndices[i])
            if (movedNode) {
              const oldIndex = oldIndices.find(i => i !== newIndices[sortedNodes.indexOf(movedNode)])
              const newIndex = newIndices.find(i => i !== oldIndices[sortedNodes.indexOf(movedNode)])
              if (oldIndex !== undefined && newIndex !== undefined) {
                handleDragEnd(oldIndex, newIndex)
              }
            }
          }}
          className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4"
        >
          {sortedNodes.map((node, index) => (
            <NodeCard
              key={node.id}
              node={node}
              isActive={activeNodeId === node.id}
              onClick={handleNodeSwitch}
              index={index}
            />
          ))}
        </Reorder.Group>

        {sortedNodes.length === 0 && (
          <div className="text-center py-12 border-2 border-dashed border-border rounded-xl">
            <div className="text-muted-foreground mb-4">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="48"
                height="48"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <rect width="18" height="15" x="3" y="4" rx="2" ry="2" />
                <line x1="2" x2="22" y1="20" y2="20" />
                <line x1="4" x2="8" y1="20" y2="20" />
              </svg>
            </div>
            <h3 className="text-lg font-semibold mb-2">No nodes found</h3>
            <p className="text-muted-foreground mb-6">
              Get started by adding a new node to your controller.
            </p>
            <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors">
              Add First Node
            </button>
          </div>
        )}
      </div>
    </div>
  )
}

// NodeCard component for the dashboard
interface NodeCardProps {
  node: NodeInfo
  isActive: boolean
  onClick: (node: NodeInfo) => void
  index: number
}

function NodeCard({ node, isActive, onClick, index }: NodeCardProps) {
  const handleCardClick = () => {
    onClick(node)
  }

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'online':
        return 'online'
      case 'offline':
        return 'offline'
      case 'connecting':
        return 'connecting'
      default:
        return 'unknown'
    }
  }

  const getStatusText = (status: string) => {
    switch (status) {
      case 'online':
        return 'Online'
      case 'offline':
        return 'Offline'
      case 'connecting':
        return 'Connecting'
      default:
        return 'Unknown'
    }
  }

  return (
    <Reorder.Item
      value={node}
      id={node.id}
      dragHandlerId={`drag-handle-${node.id}`}
      layoutId={`node-card-${node.id}`}
      className="outline-none"
    >
      <motion.div
        layoutId={`node-card-container-${node.id}`}
        onClick={handleCardClick}
        className={`
          relative group cursor-pointer rounded-xl border transition-all duration-200
          hover:shadow-lg active:scale-95
          ${isActive
            ? 'border-emerald-500 shadow-[0_0_20px_rgba(16,185,129,0.3)] ring-2 ring-emerald-500/50'
            : 'border-border bg-card hover:border-emerald-500/50'
          }
          touch-active:scale-[0.98]
        `}
        whileHover={{ y: -2 }}
        whileTap={{ scale: 0.98 }}
      >
        {/* Active indicator badge */}
        {isActive && (
          <div className="absolute -top-3 right-4">
            <span className="px-3 py-1 text-xs font-medium bg-emerald-500 text-white rounded-full shadow-md">
              Active
            </span>
          </div>
        )}

        {/* Card header */}
        <div className="flex items-start justify-between p-4 pb-3">
          <div className="flex items-center gap-3">
            {/* Node icon */}
            <div className="h-10 w-10 rounded-lg bg-gradient-to-br from-emerald-500 to-emerald-600 flex items-center justify-center text-white shadow-sm">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="20"
                height="20"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <rect width="18" height="15" x="3" y="4" rx="2" ry="2" />
                <line x1="2" x2="22" y1="20" y2="20" />
                <line x1="4" x2="8" y1="20" y2="20" />
              </svg>
            </div>

            <div>
              <h3 className="font-semibold text-base group-hover:text-emerald-500 transition-colors">
                {node.name}
              </h3>
              <div className="flex items-center gap-2 text-xs text-muted-foreground mt-1">
                <StatusDot
                  status={getStatusColor(node.status || 'unknown')}
                  size="sm"
                />
                <span>{getStatusText(node.status || 'unknown')}</span>
              </div>
            </div>
          </div>

          {/* Drag handle */}
          <div
            id={`drag-handle-${node.id}`}
            className="opacity-0 group-hover:opacity-100 transition-opacity cursor-grab active:cursor-grabbing text-muted-foreground"
          >
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <circle cx="9" cy="6" r="1" />
              <circle cx="9" cy="12" r="1" />
              <circle cx="9" cy="18" r="1" />
              <circle cx="15" cy="6" r="1" />
              <circle cx="15" cy="12" r="1" />
              <circle cx="15" cy="18" r="1" />
            </svg>
          </div>
        </div>

        {/* Card body */}
        <div className="px-4 pb-4 space-y-2">
          {/* Machine info */}
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <rect width="18" height="12" x="3" y="3" rx="2" />
              <path d="M2 10h20" />
            </svg>
            <span className="truncate" title={node.hostname}>
              {node.hostname || node.address || 'Unknown machine'}
            </span>
          </div>

          {/* IP address */}
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="12"
              height="12"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M22 12h-4l-3 9L9 3l-3 9H2" />
            </svg>
            <span className="font-mono">{node.ip_address || 'N/A'}</span>
          </div>

          {/* Machine class badge */}
          {node.machine_class && (
            <div className="flex items-center gap-2 mt-2">
              <span className="px-2 py-1 text-xs font-medium bg-secondary text-secondary-foreground rounded-md">
                {node.machine_class === 'kiosk' ? 'Kiosk' : node.machine_class === 'server' ? 'Server' : 'Workstation'}
              </span>
            </div>
          )}
        </div>

        {/* Video thumbnail placeholder */}
        {node.status === 'online' && (
          <div className="relative h-24 w-full bg-muted/30 border-t border-border/50">
            <div className="absolute inset-0 flex items-center justify-center">
              <div className="relative w-full h-full overflow-hidden">
                <div className="absolute inset-0 bg-gradient-to-t from-black/50 to-transparent" />
                <div className="absolute inset-0 flex items-center justify-center">
                  <div className="h-12 w-12 rounded-full bg-black/30 flex items-center justify-center backdrop-blur-sm">
                    <svg
                      xmlns="http://www.w3.org/2000/svg"
                      width="24"
                      height="24"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      className="text-white"
                    >
                      <polygon points="23 7 16 12 23 17 23 7" />
                      <rect width="16" height="14" x="1" y="5" rx="2" ry="2" />
                    </svg>
                  </div>
                </div>
              </div>
            </div>
            <div className="absolute bottom-2 right-2 px-2 py-1 bg-black/60 text-white text-xs rounded">
              Video
            </div>
          </div>
        )}

        {/* Quick switch hint */}
        <div className="px-4 pb-3 pt-2 flex items-center justify-center text-xs text-muted-foreground opacity-60">
          <svg
            xmlns="http://www.w3.org/2000/svg"
            width="12"
            height="12"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            className="mr-1"
          >
            <path d="m10 13 5 7L5 20" />
            <path d="m14 7 5 7L19 7" />
            <path d="M5 20 14 13" />
            <path d="m19 7-5 7L5 7" />
          </svg>
          Click to switch
        </div>
      </motion.div>
    </Reorder.Item>
  )
}
