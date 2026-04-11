import { motion, Reorder } from 'framer-motion'
import { StatusDot } from './StatusDot'
import { NodeInfo } from '../types/node'

interface NodeCardProps {
  node: NodeInfo
  isActive: boolean
  onClick: (node: NodeInfo) => void
  onDragEnd?: (oldIndex: number, newIndex: number) => void
}

export function NodeCard({ node, isActive, onClick, onDragEnd }: NodeCardProps) {
  const handleDragEnd = (oldIndex: number, newIndex: number) => {
    if (onDragEnd) {
      onDragEnd(oldIndex, newIndex)
    }
  }

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
      onDragEnd={() => {
        // Drag end is handled by Reorder.Group
      }}
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
          <div className="flex items-center gap-2 mt-2">
            <span className="px-2 py-1 text-xs font-medium bg-secondary text-secondary-foreground rounded-md">
              {node.machine_class === 'kiosk' ? 'Kiosk' : node.machine_class === 'server' ? 'Server' : 'Workstation'}
            </span>
          </div>
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

export default NodeCard
