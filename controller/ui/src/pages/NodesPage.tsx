import React, { useEffect } from 'react';
import { Server, Plus, RefreshCw, Search, AlertCircle } from 'lucide-react';
import { useNodesStore } from '../hooks/useNodes';
import { NodeInfo } from '../types/node';

const NodesPage: React.FC = () => {
  const store = useNodesStore();
  const { nodes, loading, error, selectedNodeId, webSocketStatus } = store;
  const { fetchNodes, selectNode } = store;
  
  const [filter, setFilter] = React.useState('');
  const [statusFilter, setStatusFilter] = React.useState<string>('all');

  const filteredNodes = nodes.filter((node) => {
    const matchesSearch =
      node.name?.toLowerCase().includes(filter.toLowerCase()) ||
      node.hostname?.toLowerCase().includes(filter.toLowerCase()) ||
      node.id?.toLowerCase().includes(filter.toLowerCase());
    const matchesStatus = statusFilter === 'all' || node.status === statusFilter;
    return matchesSearch && matchesStatus;
  });

  const handleRefresh = () => {
    fetchNodes();
  };

  // Auto-refresh when WebSocket reconnects
  useEffect(() => {
    if (webSocketStatus === 'connected') {
      fetchNodes();
    }
  }, [webSocketStatus, fetchNodes]);

  const getStatusColor = (status: NodeInfo['status']) => {
    switch (status) {
      case 'online':
        return 'bg-emerald-500';
      case 'offline':
        return 'bg-gray-500';
      case 'connecting':
        return 'bg-amber-500';
      case 'error':
        return 'bg-red-500';
      default:
        return 'bg-gray-500';
    }
  };

  const getMachineClassColor = (machineClass: NodeInfo['machine_class']) => {
    switch (machineClass) {
      case 'workstation':
        return 'text-blue-400 bg-blue-500/10';
      case 'server':
        return 'text-purple-400 bg-purple-500/10';
      case 'kiosk':
        return 'text-emerald-400 bg-emerald-500/10';
      default:
        return 'text-gray-400 bg-gray-500/10';
    }
  };

  if (loading && nodes.length === 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="flex flex-col items-center gap-4">
          <div className="w-12 h-12 border-4 border-emerald-500 border-t-transparent rounded-full animate-spin" />
          <span className="text-gray-400">Loading nodes...</span>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <AlertCircle className="w-16 h-16 text-red-500 mx-auto mb-4" />
          <h3 className="text-lg font-semibold text-gray-200 mb-2">Failed to load nodes</h3>
          <p className="text-gray-400 mb-4">{error}</p>
          <button
            onClick={handleRefresh}
            className="px-4 py-2 bg-emerald-600 hover:bg-emerald-700 text-white rounded-lg transition-colors flex items-center gap-2 mx-auto"
          >
            <RefreshCw className="w-4 h-4" />
            Retry
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex flex-col md:flex-row md:items-center justify-between gap-4">
        <div>
          <h2 className="text-2xl font-bold text-white">Nodes</h2>
          <p className="text-gray-400 mt-1">
            {nodes.length} node{nodes.length !== 1 && 's'} total
          </p>
        </div>
        <div className="flex items-center gap-3">
          <div className="flex items-center gap-2 bg-gray-800 rounded-lg p-1">
            {['all', 'online', 'offline'].map((status) => (
              <button
                key={status}
                onClick={() => setStatusFilter(status)}
                className={`px-4 py-2 rounded-md text-sm font-medium transition-colors ${
                  statusFilter === status
                    ? 'bg-emerald-500 text-white'
                    : 'text-gray-400 hover:text-gray-200'
                }`}
              >
                {status.charAt(0).toUpperCase() + status.slice(1)}
              </button>
            ))}
          </div>
          <button
            onClick={handleRefresh}
            disabled={loading}
            className="p-2 bg-gray-800 hover:bg-gray-700 text-gray-300 rounded-lg transition-colors disabled:opacity-50"
            aria-label="Refresh nodes"
          >
            <RefreshCw className={`w-5 h-5 ${loading ? 'animate-spin' : ''}`} />
          </button>
          <button className="px-4 py-2 bg-emerald-600 hover:bg-emerald-700 text-white rounded-lg transition-colors flex items-center gap-2">
            <Plus className="w-4 h-4" />
            Add Node
          </button>
        </div>
      </div>

      {/* Search */}
      <div className="relative">
        <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-gray-400" />
        <input
          type="text"
          placeholder="Search nodes by name, hostname, or ID..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="w-full pl-10 pr-4 py-3 bg-gray-800 border border-gray-700 rounded-lg text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-emerald-500 focus:ring-1 focus:ring-emerald-500"
        />
      </div>

      {/* Nodes Grid */}
      {filteredNodes.length === 0 ? (
        <div className="flex flex-col items-center justify-center py-12 text-center">
          <div className="w-16 h-16 bg-gray-800 rounded-full flex items-center justify-center mb-4">
            <Server className="w-8 h-8 text-gray-500" />
          </div>
          <h3 className="text-lg font-semibold text-gray-200">No nodes found</h3>
          <p className="text-gray-400 mt-2 max-w-md">
            {filter || statusFilter !== 'all'
              ? 'Try adjusting your search or filters'
              : 'No nodes registered with the controller yet'}
          </p>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
          {filteredNodes.map((node) => (
            <div
              key={node.id}
              onClick={() => selectNode(node.id)}
              className={`group relative p-4 bg-gray-800 border rounded-xl transition-all duration-200 cursor-pointer hover:shadow-lg hover:shadow-emerald-500/5 ${
                selectedNodeId === node.id
                  ? 'border-emerald-500 ring-1 ring-emerald-500'
                  : 'border-gray-700 hover:border-emerald-500/50'
              }`}
            >
              {/* Node status indicator */}
              <div className="absolute top-4 right-4">
                <div className="flex items-center gap-2">
                  <div className={`w-2.5 h-2.5 rounded-full ${getStatusColor(node.status)}`} />
                  <span className="text-xs font-medium text-gray-400 uppercase tracking-wider">
                    {node.status}
                  </span>
                </div>
              </div>

              <div className="flex items-start gap-3">
                <div className="w-12 h-12 bg-gray-700 rounded-lg flex items-center justify-center flex-shrink-0">
                  <Server className="w-6 h-6 text-gray-300 group-hover:text-emerald-500 transition-colors" />
                </div>
                <div className="flex-1 min-w-0">
                  <h3 className="font-semibold text-white truncate">{node.name}</h3>
                  <p className="text-sm text-gray-400 truncate">{node.hostname}</p>
                  <p className="text-xs text-gray-500 mt-1">{node.id}</p>
                </div>
              </div>

              <div className="mt-4 flex items-center justify-between text-xs text-gray-400">
                <div className="flex items-center gap-3">
                  <span className="flex items-center gap-1">
                    <Server className="w-3 h-3" />
                    {node.machine_class}
                  </span>
                  {node.ip_address && (
                    <span className="text-gray-500">{node.ip_address}</span>
                  )}
                </div>
                <div className={`px-2 py-0.5 rounded-full text-xs font-medium ${getMachineClassColor(node.machine_class)}`}>
                  {node.machine_class}
                </div>
              </div>

              {/* Active indicator */}
              {node.active && (
                <div className="absolute -top-2 -right-2 w-6 h-6 bg-emerald-500 rounded-full flex items-center justify-center shadow-lg">
                  <div className="w-2.5 h-2.5 bg-white rounded-full animate-pulse" />
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
};

export default NodesPage;
