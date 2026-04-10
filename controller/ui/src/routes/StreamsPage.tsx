import { useState } from 'react'

export default function StreamsPage() {

  const streams = [
    { id: 'node1', name: 'Workstation A', node: 'workstation-a', type: 'HLS', status: 'active' },
    { id: 'node2', name: 'Workstation B', node: 'workstation-b', type: 'MJPEG', status: 'idle' },
    { id: 'node3', name: 'Server C', node: 'server-c', type: 'HLS', status: 'idle' },
  ]

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Video Streams</h1>
        <p className="text-slate-400 text-sm mt-1">
          Monitor and manage video streams from nodes
        </p>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {streams.map((stream) => (
          <div
            key={stream.id}
            className="bg-slate-900 rounded-xl border border-slate-800 overflow-hidden"
          >
            <div className="p-5 border-b border-slate-800 flex items-center justify-between">
              <div>
                <h3 className="font-semibold text-slate-100">{stream.name}</h3>
                <p className="text-xs text-slate-500">Node: {stream.node}</p>
              </div>
              <div className="flex items-center gap-2">
                <span className={`px-2 py-1 rounded-md text-xs font-medium ${
                  stream.status === 'active' ? 'bg-emerald-500/20 text-emerald-400' : 'bg-slate-700 text-slate-300'
                }`}>
                  {stream.status.toUpperCase()}
                </span>
                <span className="px-2 py-1 rounded-md text-xs font-mono bg-slate-800 text-slate-400">
                  {stream.type}
                </span>
              </div>
            </div>
            <div className="aspect-video bg-slate-950 flex items-center justify-center">
              <div className="text-center">
                <svg className="w-12 h-12 mx-auto text-slate-700 mb-2" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 10l4.553-2.276A1 1 0 0121 8.618v6.764a1 1 0 01-1.447.894L15 14M5 18h8a2 2 0 002-2V8a2 2 0 00-2-2H5a2 2 0 00-2 2v8a2 2 0 002 2z" />
                </svg>
                <p className="text-sm text-slate-600">Stream not connected</p>
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}
