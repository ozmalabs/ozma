import { useParams, useNavigate } from 'react-router-dom'
import { useNodes } from '../hooks/useNodes'
import {
  ServerIcon, CpuIcon, ActivityIcon, Server as ServerType,
  WifiIcon, ClockIcon, ChevronLeftIcon
} from 'lucide-react'
import { NodeInfo } from '../types'

const NodeDetailPage = () => {
  const { nodeId } = useParams()
  const navigate = useNavigate()
  const { nodes, activeNodeId } = useNodes()

  const node = nodes.find(n => n.id === nodeId)

  if (!node) {
    return (
      <div className="max-w-4xl mx-auto">
        <button
          onClick={() => navigate('/')}
          className="mb-6 flex items-center gap-2 text-text-secondary hover:text-text-primary transition-colors"
        >
          <ChevronLeftIcon className="w-4 h-4" />
          Back to Nodes
        </button>
        <div className="bg-secondary rounded-lg border border-border p-8 text-center">
          <NodeIcon className="w-12 h-12 text-text-tertiary mx-auto mb-4" />
          <h3 className="text-lg font-medium text-text-primary">Node Not Found</h3>
          <p className="text-text-tertiary mt-2">The node you're looking for doesn't exist or has been removed</p>
          <button
            onClick={() => navigate('/')}
            className="mt-6 px-4 py-2 bg-brand-accent text-black font-medium rounded-md hover:bg-brand-accent-hover transition-colors"
          >
            Back to Nodes
          </button>
        </div>
      </div>
    )
  }

  const isActive = node.id === activeNodeId

  const getStatusColor = (status: NodeInfo['status']) => {
    switch (status) {
      case 'online':
        return 'bg-success'
      case 'offline':
        return 'bg-error'
      case 'connecting':
        return 'bg-warning animate-pulse'
      case 'error':
        return 'bg-error'
      default:
        return 'bg-text-tertiary'
    }
  }

  return (
    <div className="max-w-4xl mx-auto">
      <button
        onClick={() => navigate('/')}
        className="mb-6 flex items-center gap-2 text-text-secondary hover:text-text-primary transition-colors"
      >
        <ChevronLeftIcon className="w-4 h-4" />
        Back to Nodes
      </button>

      <div className="grid grid-cols-1 md:grid-cols-3 gap-6">
        {/* Header Card */}
        <div className="md:col-span-3 bg-secondary rounded-lg border border-border overflow-hidden">
          <div className="p-6 border-b border-border flex items-start justify-between">
            <div className="flex items-center gap-4">
              <div className={`w-14 h-14 rounded-lg flex items-center justify-center ${isActive ? 'bg-brand-accent text-black' : 'bg-secondary text-text-primary'}`}>
                <NodeIcon className="w-8 h-8" />
              </div>
              <div>
                <div className="flex items-center gap-3">
                  <h2 className="text-xl font-bold text-text-primary">{node.name}</h2>
                  {isActive && (
                    <span className="bg-brand-accent text-black text-xs font-bold px-2 py-0.5 rounded-full">
                      Active
                    </span>
                  )}
                </div>
                <p className="text-text-tertiary mt-1 font-mono text-sm">ID: {node.id}</p>
              </div>
            </div>
            <div className="text-right">
              <div className="flex items-center justify-end gap-2 mb-1">
                <span className={`w-2.5 h-2.5 rounded-full ${getStatusColor(node.status)}`}></span>
                <span className="text-sm font-medium text-text-primary">
                  {node.status.charAt(0).toUpperCase() + node.status.slice(1)}
                </span>
              </div>
              <p className="text-xs text-text-tertiary">Last seen: {new Date(node.last_seen).toLocaleString()}</p>
            </div>
          </div>

          <div className="grid grid-cols-2 md:grid-cols-4 gap-px bg-border">
            <div className="bg-secondary p-4">
              <p className="text-xs text-text-tertiary mb-1 flex items-center gap-1">
                <ServerIcon className="w-3 h-3" /> Machine Class
              </p>
              <p className="font-medium text-text-primary capitalize">{node.machine_class}</p>
            </div>
            <div className="bg-secondary p-4">
              <p className="text-xs text-text-tertiary mb-1 flex items-center gap-1">
                <CpuIcon className="w-3 h-3" /> Hostname
              </p>
              <p className="font-mono text-sm text-text-primary">{node.hostname}</p>
            </div>
            {node.ip_address && (
              <div className="bg-secondary p-4">
                <p className="text-xs text-text-tertiary mb-1 flex items-center gap-1">
                  <ActivityIcon className="w-3 h-3" /> IP Address
                </p>
                <p className="font-mono text-sm text-text-primary">{node.ip_address}</p>
              </div>
            )}
            <div className="bg-secondary p-4">
              <p className="text-xs text-text-tertiary mb-1 flex items-center gap-1">
                <ClockIcon className="w-3 h-3" /> Registered
              </p>
              <p className="text-sm text-text-primary">Unknown</p>
            </div>
          </div>
        </div>

        {/* Capabilities */}
        <div className="md:col-span-3 bg-secondary rounded-lg border border-border">
          <div className="p-6 border-b border-border">
            <h3 className="font-semibold text-text-primary">Capabilities</h3>
          </div>
          <div className="p-6 grid grid-cols-3 gap-4">
            <div className="flex items-center gap-3 p-3 rounded-lg bg-text-tertiary/5">
              <div className={`w-8 h-8 rounded-md flex items-center justify-center ${node.capabilities.hid ? 'bg-brand-accent/20 text-brand-accent' : 'bg-error/10 text-error'}`}>
                <NodeIcon className="w-4 h-4" />
              </div>
              <div>
                <p className="text-sm font-medium text-text-primary">HID</p>
                <p className="text-xs text-text-tertiary">
                  {node.capabilities.hid ? 'Enabled' : 'Disabled'}
                </p>
              </div>
            </div>
            <div className="flex items-center gap-3 p-3 rounded-lg bg-text-tertiary/5">
              <div className={`w-8 h-8 rounded-md flex items-center justify-center ${node.capabilities.video ? 'bg-brand-accent/20 text-brand-accent' : 'bg-error/10 text-error'}`}>
                <ActivityIcon className="w-4 h-4" />
              </div>
              <div>
                <p className="text-sm font-medium text-text-primary">Video</p>
                <p className="text-xs text-text-tertiary">
                  {node.capabilities.video ? 'Enabled' : 'Disabled'}
                </p>
              </div>
            </div>
            <div className="flex items-center gap-3 p-3 rounded-lg bg-text-tertiary/5">
              <div className={`w-8 h-8 rounded-md flex items-center justify-center ${node.capabilities.audio ? 'bg-brand-accent/20 text-brand-accent' : 'bg-error/10 text-error'}`}>
                <WifiIcon className="w-4 h-4" />
              </div>
              <div>
                <p className="text-sm font-medium text-text-primary">Audio</p>
                <p className="text-xs text-text-tertiary">
                  {node.capabilities.audio ? 'Enabled' : 'Disabled'}
                </p>
              </div>
            </div>
          </div>
        </div>

        {/* Metadata (if present) */}
        {node.metadata && Object.keys(node.metadata).length > 0 && (
          <div className="md:col-span-3 bg-secondary rounded-lg border border-border">
            <div className="p-6 border-b border-border">
              <h3 className="font-semibold text-text-primary">Metadata</h3>
            </div>
            <div className="p-6">
              <dl className="grid grid-cols-1 md:grid-cols-2 gap-4">
                {Object.entries(node.metadata).map(([key, value]) => (
                  <div key={key} className="flex flex-col">
                    <dt className="text-xs text-text-tertiary uppercase tracking-wider mb-1">{key}</dt>
                    <dd className="text-sm font-medium text-text-primary">
                      {typeof value === 'object' ? JSON.stringify(value) : String(value)}
                    </dd>
                  </div>
                ))}
              </dl>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

export default NodeDetailPage
