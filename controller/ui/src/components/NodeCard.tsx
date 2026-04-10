import { Link } from 'react-router-dom'
import type { NodeInfo } from '../types'

interface NodeCardProps {
  node: NodeInfo
}

export default function NodeCard({ node }: NodeCardProps) {
  const statusColor =
    node.status === 'online'
      ? 'bg-emerald-500'
      : node.status === 'offline'
      ? 'bg-slate-500'
      : 'bg-amber-500'

  const machineClassColor =
    node.machine_class === 'server'
      ? 'bg-blue-500/20 text-blue-400'
      : node.machine_class === 'kiosk'
      ? 'bg-amber-500/20 text-amber-400'
      : 'bg-slate-500/20 text-slate-400'

  return (
    <Link
      to={`/nodes/${node.id}`}
      className="group relative flex flex-col rounded-xl bg-slate-900 border border-slate-800 hover:border-emerald-500/50 transition-all duration-300 hover:shadow-lg hover:shadow-emerald-500/10 overflow-hidden"
    >
      <div className="p-5">
        <div className="flex items-start justify-between mb-4">
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 rounded-lg bg-gradient-to-br from-emerald-500/20 to-teal-500/20 border border-emerald-500/30 flex items-center justify-center text-emerald-400 group-hover:scale-110 transition-transform">
              <svg className="w-6 h-6" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 12h14M5 12a2 2 0 01-2-2 2 2 0 00-2 2 2 2 0 002 2 2 2 0 012 2v2a2 2 0 01-2 2 2 2 0 00-2-2 2 2 0 002 2 2 2 0 012-2h14a2 2 0 012 2 2 2 0 002-2 2 2 0 00-2-2 2 2 0 01-2-2v-2a2 2 0 012-2 2 2 0 002 2 2 2 0 00-2-2 2 2 0 01-2 2H5z" />
              </svg>
            </div>
            <div>
              <h3 className="font-semibold text-slate-100 group-hover:text-emerald-400 transition-colors">
                {node.name}
              </h3>
              <p className="text-xs text-slate-500 font-mono">{node.hostname}</p>
            </div>
          </div>
          <span className={`px-2 py-1 rounded-full text-xs font-medium ${machineClassColor} capitalize`}>
            {node.machine_class}
          </span>
        </div>

        <div className="space-y-2 text-sm text-slate-400">
          <div className="flex items-center gap-2">
            <svg className="w-4 h-4 text-slate-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 12a9 9 0 01-9 9m9-9a9 9 0 00-9-9m9 9H3m9 9a9 9 0 01-9-9m9 9c1.657 0 3-4.03 3-9s-1.343-9-3-9m0 18c-1.657 0-3-4.03-3-9s1.343-9 3-9m-9 9a9 9 0 019-9" />
            </svg>
            <span className="font-mono text-xs">{node.ip_address}</span>
          </div>
          <div className="flex items-center gap-2">
            <svg className="w-4 h-4 text-slate-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 8v4l3 3m6-3a9 9 0 11-18 0 9 9 0 0118 0z" />
            </svg>
            <span className="text-xs">Last seen: {new Date(node.last_seen).toLocaleString()}</span>
          </div>
        </div>

        {node.active && (
          <div className="mt-4 flex items-center gap-2 text-emerald-400 bg-emerald-500/10 rounded-lg p-2">
            <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 12l2 2 4-4m6 2a9 9 0 11-18 0 9 9 0 0118 0z" />
            </svg>
            <span className="text-xs font-medium">Currently Active</span>
          </div>
        )}
      </div>

      <div className="px-5 py-3 border-t border-slate-800 bg-slate-900/50 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <span className={`w-2 h-2 rounded-full ${statusColor} ${node.status === 'connecting' ? 'animate-pulse' : ''}`} />
          <span className="text-xs font-medium capitalize text-slate-300">{node.status}</span>
        </div>
        <div className="flex items-center gap-1 text-slate-500">
          <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
          </svg>
        </div>
      </div>
    </Link>
  )
}
