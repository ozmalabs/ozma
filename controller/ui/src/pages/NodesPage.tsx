import { useEffect } from 'react'
import { useNodeStore } from '../store/useNodeStore'
import { LayoutDashboard, Activity, Server, Settings, Search } from 'lucide-react'
import { Link } from 'react-router-dom'

const NodesPage = () => {
  const { nodes, loading, error, fetchNodes, connectWebSocket } = useNodeStore()

  useEffect(() => {
    fetchNodes()
    connectWebSocket()
  }, [fetchNodes, connectWebSocket])

  const getNodeStatus = (node: any) => {
    switch (node.status) {
      case 'online':
        return {
          text: 'Online',
          color: 'text-emerald-500',
          bg: 'bg-emerald-500/10',
          border: 'border-emerald-500/20',
          dot: 'bg-emerald-500',
        }
      case 'offline':
        return {
          text: 'Offline',
          color: 'text-slate-500',
          bg: 'bg-slate-500/10',
          border: 'border-slate-500/20',
          dot: 'bg-slate-500',
        }
      case 'connecting':
        return {
          text: 'Connecting',
          color: 'text-blue-500',
          bg: 'bg-blue-500/10',
          border: 'border-blue-500/20',
          dot: 'bg-blue-500 animate-pulse',
        }
      case 'error':
        return {
          text: 'Error',
          color: 'text-red-500',
          bg: 'bg-red-500/10',
          border: 'border-red-500/20',
          dot: 'bg-red-500',
        }
      default:
        return {
          text: 'Unknown',
          color: 'text-slate-400',
          bg: 'bg-slate-500/10',
          border: 'border-slate-500/20',
          dot: 'bg-slate-400',
        }
    }
  }

  const getNodeClassColor = (machineClass: string) => {
    switch (machineClass) {
      case 'workstation':
        return 'text-blue-400'
      case 'server':
        return 'text-purple-400'
      case 'kiosk':
        return 'text-amber-400'
      default:
        return 'text-slate-400'
    }
  }

  const stats = {
    total: nodes.length,
    online: nodes.filter((n) => n.status === 'online').length,
    active: nodes.filter((n) => n.active).length,
  }

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="w-12 h-12 border-4 border-emerald-500/30 border-t-emerald-500 rounded-full animate-spin mx-auto mb-4" />
          <p className="text-slate-500">Loading nodes...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <p className="text-red-500 mb-4">Error: {error}</p>
          <button
            onClick={() => fetchNodes()}
            className="px-4 py-2 bg-emerald-600 hover:bg-emerald-700 text-white rounded-lg transition-colors"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="max-w-7xl mx-auto space-y-6">
      {/* Header */}
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
        <div>
          <h1 className="text-3xl font-bold text-white">Nodes</h1>
          <p className="text-slate-400">Manage and monitor your KVMA nodes</p>
        </div>
        <div className="flex gap-2">
          <button className="px-4 py-2 bg-emerald-600 hover:bg-emerald-700 text-white rounded-lg font-medium transition-colors flex items-center gap-2">
            <Activity size={18} />
            Add Node
          </button>
        </div>
      </div>

      {/* Stats cards */}
      <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
        <div className="bg-slate-800/50 border border-slate-700 rounded-xl p-5">
          <div className="flex items-center justify-between mb-2">
            <h3 className="text-slate-400 text-sm font-medium">Total Nodes</h3>
            <LayoutDashboard className="text-slate-500" size={20} />
          </div>
          <p className="text-3xl font-bold text-white">{stats.total}</p>
          <p className="text-xs text-slate-500 mt-1">
            {stats.total === 1 ? 'node' : 'nodes'} registered
          </p>
        </div>

        <div className="bg-slate-800/50 border border-slate-700 rounded-xl p-5">
          <div className="flex items-center justify-between mb-2">
            <h3 className="text-slate-400 text-sm font-medium">Online</h3>
            <Activity className="text-emerald-500" size={20} />
          </div>
          <p className="text-3xl font-bold text-emerald-500">{stats.online}</p>
          <p className="text-xs text-slate-500 mt-1">
            {stats.online === 1 ? 'node' : 'nodes'} online
          </p>
        </div>

        <div className="bg-slate-800/50 border border-slate-700 rounded-xl p-5">
          <div className="flex items-center justify-between mb-2">
            <h3 className="text-slate-400 text-sm font-medium">Active Session</h3>
            <Server className="text-blue-500" size={20} />
          </div>
          <p className="text-3xl font-bold text-blue-500">{stats.active}</p>
          <p className="text-xs text-slate-500 mt-1">
            {stats.active === 1 ? 'node' : 'nodes'} receiving input
          </p>
        </div>
      </div>

      {/* Search and filters */}
      <div className="flex flex-col sm:flex-row gap-4">
        <div className="relative flex-1">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-500" size={18} />
          <input
            type="text"
            placeholder="Search nodes by name, hostname, or IP..."
            className="w-full pl-10 pr-4 py-2.5 bg-slate-800 border border-slate-700 rounded-lg text-sm text-white placeholder-slate-500 focus:outline-none focus:border-emerald-500 focus:ring-1 focus:ring-emerald-500 transition-colors"
          />
        </div>
        <div className="flex gap-2">
          <select className="px-4 py-2.5 bg-slate-800 border border-slate-700 rounded-lg text-sm text-slate-300 focus:outline-none focus:border-emerald-500 focus:ring-1 focus:ring-emerald-500 transition-colors">
            <option>All Status</option>
            <option>Online</option>
            <option>Offline</option>
            <option>Connecting</option>
            <option>Error</option>
          </select>
          <select className="px-4 py-2.5 bg-slate-800 border border-slate-700 rounded-lg text-sm text-slate-300 focus:outline-none focus:border-emerald-500 focus:ring-1 focus:ring-emerald-500 transition-colors">
            <option>All Classes</option>
            <option>Workstation</option>
            <option>Server</option>
            <option>Kiosk</option>
          </select>
        </div>
      </div>

      {/* Nodes table */}
      <div className="bg-slate-800/50 border border-slate-700 rounded-xl overflow-hidden">
        {nodes.length === 0 ? (
          <div className="p-12 text-center">
            <div className="w-16 h-16 bg-slate-700/50 rounded-full flex items-center justify-center mx-auto mb-4">
              <LayoutDashboard className="text-slate-500" size={32} />
            </div>
            <h3 className="text-lg font-medium text-white mb-2">No nodes found</h3>
            <p className="text-slate-500 mb-6">Get started by adding your first node</p>
            <button className="px-6 py-2 border border-dashed border-emerald-500/50 text-emerald-500 hover:bg-emerald-500/10 rounded-lg transition-colors">
              Register New Node
            </button>
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full">
              <thead>
                <tr className="border-b border-slate-700 bg-slate-800/80">
                  <th className="px-6 py-4 text-left text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    Node
                  </th>
                  <th className="px-6 py-4 text-left text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    Status
                  </th>
                  <th className="px-6 py-4 text-left text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    Machine Class
                  </th>
                  <th className="px-6 py-4 text-left text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    IP Address
                  </th>
                  <th className="px-6 py-4 text-left text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    CPU
                  </th>
                  <th className="px-6 py-4 text-left text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    Memory
                  </th>
                  <th className="px-6 py-4 text-left text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    Last Seen
                  </th>
                  <th className="px-6 py-4 text-right text-xs font-semibold text-slate-400 uppercase tracking-wider">
                    Actions
                  </th>
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-700">
                {nodes.map((node) => {
                  const status = getNodeStatus(node)
                  return (
                    <tr
                      key={node.id}
                      className="hover:bg-slate-700/30 transition-colors group cursor-pointer"
                    >
                      <td className="px-6 py-4">
                        <div className="flex items-center gap-3">
                          <div className="w-10 h-10 rounded-lg bg-slate-700/50 flex items-center justify-center group-hover:bg-slate-700 transition-colors">
                            {node.machine_class === 'kiosk' ? (
                              <Activity size={20} className="text-amber-400" />
                            ) : node.machine_class === 'server' ? (
                              <Server size={20} className="text-purple-400" />
                            ) : (
                              <LayoutDashboard size={20} className="text-blue-400" />
                            )}
                          </div>
                          <div>
                            <div className="flex items-center gap-2">
                              <span className="font-medium text-white">{node.name}</span>
                              {node.active && (
                                <span className="w-2 h-2 bg-emerald-500 rounded-full" title="Active node" />
                              )}
                            </div>
                            <p className="text-xs text-slate-500">{node.hostname}</p>
                          </div>
                        </div>
                      </td>
                      <td className="px-6 py-4">
                        <span
                          className={`px-2.5 py-1 rounded-full text-xs font-medium border ${status.bg} ${status.border} ${status.color}`}
                        >
                          {status.text}
                        </span>
                      </td>
                      <td className="px-6 py-4">
                        <span className={`text-sm ${getNodeClassColor(node.machine_class)}`}>
                          {node.machine_class.charAt(0).toUpperCase() +
                            node.machine_class.slice(1)}
                        </span>
                      </td>
                      <td className="px-6 py-4">
                        <div>
                          <div className="text-sm text-slate-300">{node.ip}</div>
                          <div className="text-xs text-slate-500">{node.mac}</div>
                        </div>
                      </td>
                      <td className="px-6 py-4">
                        <div className="flex items-center gap-2">
                          <div className="w-20 h-2 bg-slate-700 rounded-full overflow-hidden">
                            <div
                              className={`h-full rounded-full ${
                                node.cpu_usage > 80
                                  ? 'bg-red-500'
                                  : node.cpu_usage > 50
                                  ? 'bg-amber-500'
                                  : 'bg-emerald-500'
                              }`}
                              style={{ width: `${node.cpu_usage}%` }}
                            />
                          </div>
                          <span className="text-xs text-slate-400">{node.cpu_usage}%</span>
                        </div>
                      </td>
                      <td className="px-6 py-4">
                        <div className="flex items-center gap-2">
                          <div className="w-20 h-2 bg-slate-700 rounded-full overflow-hidden">
                            <div
                              className={`h-full rounded-full ${
                                node.memory_usage > 80
                                  ? 'bg-red-500'
                                  : node.memory_usage > 50
                                  ? 'bg-amber-500'
                                  : 'bg-blue-500'
                              }`}
                              style={{ width: `${node.memory_usage}%` }}
                            />
                          </div>
                          <span className="text-xs text-slate-400">{node.memory_usage}%</span>
                        </div>
                      </td>
                      <td className="px-6 py-4">
                        <div className="text-sm text-slate-300">
                          {new Date(node.last_seen).toLocaleString()}
                        </div>
                        <div className="text-xs text-slate-500">
                          Uptime: {Math.floor(node.uptime / 3600)}h
                        </div>
                      </td>
                      <td className="px-6 py-4 text-right">
                        <div className="flex items-center justify-end gap-2 opacity-0 group-hover:opacity-100 transition-opacity">
                          <button className="p-1.5 text-slate-400 hover:text-white hover:bg-slate-700 rounded transition-colors">
                            <Activity size={16} />
                          </button>
                          <button className="p-1.5 text-slate-400 hover:text-white hover:bg-slate-700 rounded transition-colors">
                            <Settings size={16} />
                          </button>
                        </div>
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}

        {/* Pagination */}
        {nodes.length > 0 && (
          <div className="flex items-center justify-between px-6 py-4 border-t border-slate-700">
            <div className="text-sm text-slate-500">
              Showing {nodes.length} of {nodes.length} nodes
            </div>
            <div className="flex gap-2">
              <button className="px-3 py-1.5 text-sm text-slate-400 hover:text-white bg-slate-800 border border-slate-700 rounded hover:bg-slate-700 disabled:opacity-50 disabled:cursor-not-allowed transition-colors">
                Previous
              </button>
              <button className="px-3 py-1.5 text-sm text-slate-400 hover:text-white bg-slate-800 border border-slate-700 rounded hover:bg-slate-700 disabled:opacity-50 disabled:cursor-not-allowed transition-colors">
                Next
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

export default NodesPage
