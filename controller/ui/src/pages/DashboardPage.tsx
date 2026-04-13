import { useEffect } from 'react'
import { useOzmaStore } from '../store/ozmaStore'
import { gqlFetch, GET_NODES, GET_SCENARIOS } from '../graphql/queries'
import { useWebSocket } from '../ws/useWebSocket'
import type { GetNodesData, GetScenariosData } from '../graphql/queries'

export default function DashboardPage() {
  const nodes = useOzmaStore((state) => state.nodes)
  const scenarios = useOzmaStore((state) => state.scenarios)
  const setNodes = useOzmaStore((state) => state.setNodes)
  const setScenarios = useOzmaStore((state) => state.setScenarios)
  const activeNodeId = useOzmaStore((state) => state.activeNodeId)
  const wsConnected = useOzmaStore((state) => state.wsConnected)

  // Initialize WebSocket connection
  useWebSocket()

  // Fetch initial data
  useEffect(() => {
    const fetchData = async () => {
      try {
        // Fetch nodes
        const nodesData = await gqlFetch<GetNodesData>(GET_NODES)
        setNodes(nodesData.nodes)
        
        // Fetch scenarios
        const scenariosData = await gqlFetch<GetScenariosData>(GET_SCENARIOS)
        setScenarios(scenariosData.scenarios)
      } catch (error) {
        console.error('Failed to fetch initial data:', error)
      }
    }

    fetchData()
  }, [setNodes, setScenarios])

  const activeScenario = scenarios.find(s => s.active)
  const activeNode = nodes.find(n => n.id === activeNodeId)

  return (
    <div className="dashboard">
      <h1>Dashboard</h1>
      
      <div className="connection-status">
        <span className={`status-indicator ${wsConnected ? 'connected' : 'disconnected'}`}>
          {wsConnected ? 'Connected' : 'Disconnected'}
        </span>
      </div>
      
      <div className="active-scenario">
        <h2>Active Scenario</h2>
        {activeScenario ? (
          <div className="scenario-card">
            <h3>{activeScenario.name}</h3>
            <p>ID: {activeScenario.id}</p>
          </div>
        ) : (
          <p>No active scenario</p>
        )}
      </div>
      
      <div className="active-node">
        <h2>Active Node</h2>
        {activeNode ? (
          <div className="node-card">
            <h3>{activeNode.name || activeNode.hostname}</h3>
            <p>ID: {activeNode.id}</p>
            <p>Host: {activeNode.host}:{activeNode.port}</p>
            <p>Status: {activeNode.status}</p>
          </div>
        ) : (
          <p>No active node</p>
        )}
      </div>
      
      <div className="nodes-list">
        <h2>All Nodes</h2>
        <div className="nodes-grid">
          {nodes.map(node => (
            <div 
              key={node.id} 
              className={`node-card ${node.id === activeNodeId ? 'active' : ''}`}
            >
              <h3>{node.name || node.hostname}</h3>
              <p>ID: {node.id}</p>
              <p>Host: {node.host}:{node.port}</p>
              <p>Status: {node.status}</p>
              <p>Machine: {node.machine_class}</p>
            </div>
          ))}
        </div>
      </div>
      
      <div className="scenarios-list">
        <h2>All Scenarios</h2>
        <div className="scenarios-grid">
          {scenarios.map(scenario => (
            <div 
              key={scenario.id} 
              className={`scenario-card ${scenario.active ? 'active' : ''}`}
            >
              <h3>{scenario.name}</h3>
              <p>ID: {scenario.id}</p>
              <p>Active: {scenario.active ? 'Yes' : 'No'}</p>
              {scenario.node_id && <p>Node: {scenario.node_id}</p>}
            </div>
          ))}
        </div>
      </div>
    </div>
  )
}
