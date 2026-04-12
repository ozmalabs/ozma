import Card from './Card';

export interface ScenarioInfo {
  id: string;
  name: string;
  active: boolean;
  node_count?: number;
}

interface ScenarioCardProps {
  scenario: ScenarioInfo;
  className?: string;
}

export default function ScenarioCard({ scenario, className = '' }: ScenarioCardProps) {
  return (
    <Card className={`${scenario.active ? 'ring-1 ring-emerald-400' : ''} ${className}`}>
      <div className="flex items-center justify-between mb-2">
        <span className="text-zinc-100 font-semibold truncate">{scenario.name}</span>
        {scenario.active && (
          <span className="text-xs font-medium text-emerald-400">Active</span>
        )}
      </div>
      {scenario.node_count !== undefined && (
        <div className="text-xs text-zinc-400">
          <span className="text-zinc-500">Nodes: </span>
          {scenario.node_count}
        </div>
      )}
    </Card>
  );
}
