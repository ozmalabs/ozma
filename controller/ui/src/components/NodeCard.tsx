import StatusDot from './StatusDot';
import Card from './Card';

export interface NodeInfo {
  id: string;
  name?: string;
  hostname: string;
  host: string;
  machine_class: string;
  online: boolean;
}

interface NodeCardProps {
  node: NodeInfo;
  className?: string;
}

export default function NodeCard({ node, className = '' }: NodeCardProps) {
  return (
    <Card className={className}>
      <div className="flex items-center justify-between mb-2">
        <span className="text-zinc-100 font-semibold truncate">
          {node.name ?? node.hostname}
        </span>
        <StatusDot status={node.online ? 'online' : 'offline'} />
      </div>
      <div className="text-xs text-zinc-400 space-y-1">
        <div>
          <span className="text-zinc-500">Class: </span>
          <span className="capitalize">{node.machine_class}</span>
        </div>
        <div>
          <span className="text-zinc-500">IP: </span>
          {node.host}
        </div>
      </div>
    </Card>
  );
}
