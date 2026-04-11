import { useState, useEffect, useCallback, useMemo } from 'react'
import { motion, Reorder } from 'framer-motion'
import { NodeCard } from '../components/NodeCard'
import { StatusDot } from '../components/StatusDot'
import { useNodesStore } from '../store/useNodesStore'
import { useNodesQuery, useNodeSubscription } from '../hooks/useGraphQL'
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
  const { nodes, loading, error, fetchNodes, updateNode } = useNodesStore()
  const { data: graphqlData, error: graphqlError } = useNodesQuery()

  // Track active node (for quick-switch)
  const [activeNodeId, setActiveNodeId] = useState<string | null>(null)
  const [draggedNodeId, setDraggedNodeId] = useState<string | null>(null)

  // Get node order from localStorage
  const [nodeOrder, setNodeOrder] = useState<string[]>(loadNodeOrder())

  // Handle node switch via GraphQL
  const handleNodeSwitch = useCallback((node: NodeInfo) => {
    console.log('Switching to node:', node.id, node.name)
    // In a real implementation, this would call the GraphQL mutation
    // activateNode(node.id)
    setActiveNodeId(node.id)

    // Also update local store
    updateNode({ ...node, active: true })
  }, [updateNode])

  // Handle drag and drop
  const handleDragEnd = useCallback((oldIndex: number, newIndex: number) => {
    if (newIndex >= nodes.length || oldIndex >= nodes.length) return

    const newNodes = [...nodes]
    const [removed] = newNodes.splice(oldIndex, 1)
    newNodes.splice(newIndex, 0, removed)

    // Update local state
    useNodesStore.getState().nodes = newNodes

    // Update order array
    const newOrder = newNodes.map(n => n.id)
    setNodeOrder(newOrder)
    saveNodeOrder(newOrder)
  }, [nodes])

  // Sync nodes from GraphQL subscription
  useEffect(() => {
    if (graphqlData?.nodes) {
      // Merge GraphQL data with local state
      graphqlData.nodes.forEach((graphqlNode: NodeInfo) => {
        const localNode = nodes.find(n => n.id === graphqlNode.id)
        if (localNode) {
          updateNode({ ...localNode, ...graphqlNode })
        } else {
          // Add new node
          useNodesStore.getState().addNode(graphqlNode)
        }
      })
    }
  }, [graphqlData, nodes, updateNode])

  // Sort nodes based on localStorage order
  const sortedNodes = useMemo(() => {
    if (nodeOrder.length === 0) return nodes

    const nodeMap = new Map(nodes.map(n => [n.id, n]))
    const sorted: NodeInfo[] = []
    const remaining: NodeInfo[] = []

    nodeOrder.forEach(id => {
      const node = nodeMap.get(id)
      if (node) sorted.push(node)
    })

    // Add any nodes not in order list
    nodes.forEach(node => {
      if (!nodeOrder.includes(node.id)) {
        remaining.push(node)
      }
    })

    return [...sorted, ...remaining]
  }, [nodes, nodeOrder])

  if (loading || !nodes) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
          <p className="text-muted-foreground">Loading nodes...</p>
        </div>
      </div>
    )
  }

  if (error || graphqlError) {
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
          <p className="text-muted-foreground mb-6">{error?.message || graphqlError?.message}</p>
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
              {nodes.length} node{nodes.length !== 1 ? 's' : ''} connected
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
          onReorder={handleDragEnd}
          className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4"
        >
          {sortedNodes.map((node) => (
            <NodeCard
              key={node.id}
              node={node}
              isActive={activeNodeId === node.id}
              onClick={handleNodeSwitch}
              onDragEnd={handleDragEnd}
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
