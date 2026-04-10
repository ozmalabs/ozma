import { useEffect } from 'react'
import { useNodes } from '../hooks/useNodes'

export default function NodesPage() {
  const { nodes, refreshNodes, ws } = useNodes()

  useEffect(() => {
    // Initial fetch will be triggered by store
  }, [])

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'online':
        return 'bg-emerald-500'
      case 'offline':
        return 'bg-red-500'
      case 'connecting':
        return 'bg-amber-500'
      case 'error':
        return 'bg-red-500'
      default:
        return 'bg-gray-500'
    }
  }

  const getClassColor = (classType: string) => {
    switch (classType) {
      case 'server':
        return 'bg-blue-500/20 text-blue-400 border-blue-500/30'
      case 'kiosk':
        return 'bg-purple-500/20 text-purple-400 border-purple-500/30'
      case 'workstation':
      default:
        return 'bg-emerald-500/20 text-emerald-400 border-emerald-500/30'
    }
  }

  return (
    <div className="max-w-6xl mx-auto">
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-100">Nodes</h1>
          <p className="text-sm text-gray-500 mt-1">
            {nodes.length} node{nodes.length !== 1 && 's'} registered
          </p>
        </div>
        <div className="flex items-center gap-4">
          <div className={`flex items-center gap-2 px-3 py-1.5 rounded-full text-sm ${
            wsStatus === 'connected' ? 'bg-emerald-500/10 text-emerald-400' : 'bg-red-500/10 text-red-400'
          }`}>
            <div className={`w-2 h-2 rounded-full ${
              wsStatus === 'connected' ? 'bg-emerald-500 animate-pulse' : 'bg-red-500'
            }`} />
            {wsStatus === 'connected' ? 'WebSocket Connected' : 'WebSocket Disconnected'}
          </div>
        </div>
      </div>

      {nodes.length === 0 ? (
        <div className="bg-gray-900 rounded-xl border border-gray-800 p-12 text-center">
          <div className="w-16 h-16 bg-gray-800 rounded-full flex items-center justify-center mx-auto mb-4">
            <svg className="w-8 h-8 text-gray-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 12h14M5 12a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v4a2 2 0 01-2 2M5 12a2 2 0 00-2 2v4a2 2 0 002 2h14a2 2 0 002-2v-4a2 2 0 00-2-2m-2-4h.01M17 16h.01" />
            </svg>
          </div>
          <h3 className="text-lg font-medium text-gray-300">No nodes yet</h3>
          <p className="text-sm text-gray-500 mt-2">Nodes will appear here once they register with the controller</p>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
          {nodes.map((node) => (
            <div
              key={node.id}
              className="bg-gray-900 rounded-xl border border-gray-800 hover:border-emerald-500/50 transition-colors overflow-hidden"
            >
              <div className="p-6">
                <div className="flex items-start justify-between mb-4">
                  <div className="flex items-center gap-3">
                    <div className={`w-10 h-10 rounded-lg flex items-center justify-center ${
                      node.active ? 'bg-emerald-500' : 'bg-gray-800'
                    }`}>
                      <svg className={`w-5 h-5 ${node.active ? 'text-gray-900' : 'text-gray-400'}`} fill="none" stroke="currentColor" viewBox="0 0 24 24">
                        <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 12h14M5 12a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v4a2 2 0 01-2 2M5 12a2 2 0 00-2 2v4a2 2 0 002 2h14a2 2 0 002-2v-4a2 2 0 00-2-2m-2-4h.01M17 16h.01" />
                      </svg>
                    </div>
                    <div>
                      <div className="flex items-center gap-2">
                        <h3 className="font-semibold text-gray-200">{node.name}</h3>
                        {node.active && (
                          <span className="w-2 h-2 bg-emerald-500 rounded-full" title="Active node" />
                        )}
                      </div>
                      <div className="flex items-center gap-2 mt-1">
                        <span className={`text-xs px-2 py-0.5 rounded border ${getClassColor(node.machine_class)}`}>
                          {node.machine_class}
                        </span>
                        <span className="text-xs text-gray-500">#{node.id.slice(0, 8)}</span>
                      </div>
                    </div>
                  </div>
                  <div className={`w-2 h-2 rounded-full ${getStatusColor(node.status)}`} title={node.status} />
                </div>

                <div className="space-y-3">
                  <div className="flex items-center justify-between text-sm">
                    <span className="text-gray-500">Status</span>
                    <span className={`font-medium ${node.status === 'online' ? 'text-emerald-400' : 'text-gray-300'}`}>
                      {node.status}
                    </span>
                  </div>
                  <div className="flex items-center justify-between text-sm">
                    <span className="text-gray-500">IP Address</span>
                    <span className="text-gray-300 font-mono">{node.ip}</span>
                  </div>
                  <div className="flex items-center justify-between text-sm">
                    <span className="text-gray-500">Uptime</span>
                    <span className="text-gray-300">{formatUptime(node.uptime)}</span>
                  </div>
                  <div className="flex items-center justify-between text-sm">
                    <span className="text-gray-500">CPU Usage</span>
                    <div className="flex items-center gap-2">
                      <span className="text-gray-300">{node.cpu_usage}%</span>
                      <div className="w-20 h-1.5 bg-gray-800 rounded-full overflow-hidden">
                        <div
                          className={`h-full rounded-full ${
                            node.cpu_usage > 80 ? 'bg-red-500' : 'bg-emerald-500'
                          }`}
                          style={{ width: `${node.cpu_usage}%` }}
                        />
                      </div>
                    </div>
                  </div>
                  <div className="flex items-center justify-between text-sm">
                    <span className="text-gray-500">Memory</span>
                    <div className="flex items-center gap-2">
                      <span className="text-gray-300">{node.memory_usage}%</span>
                      <div className="w-20 h-1.5 bg-gray-800 rounded-full overflow-hidden">
                        <div
                          className={`h-full rounded-full ${
                            node.memory_usage > 80 ? 'bg-red-500' : 'bg-emerald-500'
                          }`}
                          style={{ width: `${node.memory_usage}%` }}
                        />
                      </div>
                    </div>
                  </div>
                </div>

                <div className="mt-4 pt-4 border-t border-gray-800">
                  <div className="flex items-center justify-between text-sm">
                    <div>
                      <p className="text-gray-500">HID Devices</p>
                      <p className="text-gray-300 font-mono">{node.hids.length} devices</p>
                    </div>
                    <div className="text-right">
                      <p className="text-gray-500">Displays</p>
                      <p className="text-gray-300">{node.displays.length}</p>
                    </div>
                  </div>
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)

  const parts = []
  if (days > 0) parts.push(`${days}d`)
  if (hours > 0) parts.push(`${hours}h`)
  parts.push(`${minutes}m`)

  return parts.join(' ')
}
