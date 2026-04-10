import { useEffect } from 'react'
import { useNodesStore } from '../store/nodesStore'
import Layout from '../components/layout/Layout'
import NodeCard from '../components/NodeCard'

export default function NodesPage() {
  const { nodes, loading, error, fetchNodes } = useNodesStore()

  useEffect(() => {
    fetchNodes()
  }, [fetchNodes])

  return (
    <Layout>
      <div className="container mx-auto p-6">
        <div className="flex items-center justify-between mb-6">
          <h1 className="text-2xl font-bold">Nodes</h1>
          <span className="text-sm text-muted-foreground">
            {nodes.length} node{nodes.length !== 1 ? 's' : ''} registered
          </span>
        </div>

        {loading && (
          <div className="flex items-center justify-center h-64">
            <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-emerald-400" />
          </div>
        )}

        {error && (
          <div className="bg-destructive/10 p-4 rounded-lg mb-6">
            <p className="text-destructive">Error loading nodes: {error}</p>
          </div>
        )}

        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
          {nodes.map((node) => (
            <NodeCard key={node.id} node={node} />
          ))}
        </div>

        {nodes.length === 0 && !loading && !error && (
          <div className="text-center py-12">
            <p className="text-muted-foreground">No nodes registered yet</p>
          </div>
        )}
      </div>
    </Layout>
  )
}
