import { useState } from 'react'

export default function ScenariosPage() {
  const [activeScenario, setActiveScenario] = useState<string | null>(null)

  const scenarios = [
    { id: 'default', name: 'Default Setup', description: 'Standard workstation configuration', active: true },
    { id: 'presentation', name: 'Presentation Mode', description: 'Single display focus', active: false },
    { id: 'media', name: 'Media Center', description: 'Entertainment setup with audio routing', active: false },
    { id: 'development', name: 'Dev Environment', description: 'Multiple terminals and IDEs', active: false },
  ]

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Scenarios</h1>
        <p className="text-slate-400 text-sm mt-1">
          Manage and switch between preset configurations
        </p>
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
        {scenarios.map((scenario) => (
          <div
            key={scenario.id}
            onClick={() => setActiveScenario(scenario.id)}
            className={`relative p-5 rounded-xl border cursor-pointer transition-all duration-200 ${
              activeScenario === scenario.id
                ? 'bg-emerald-500/10 border-emerald-500/50'
                : 'bg-slate-900 border-slate-800 hover:border-slate-700'
            }`}
          >
            {activeScenario === scenario.id && (
              <div className="absolute top-4 right-4 w-2 h-2 rounded-full bg-emerald-500 animate-pulse" />
            )}
            <div className="flex items-center gap-3 mb-2">
              <div className="w-8 h-8 rounded-lg bg-slate-800 flex items-center justify-center">
                <svg className="w-4 h-4 text-slate-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13 10V3L4 14h7v7l9-11h-7z" />
                </svg>
              </div>
              <h3 className="font-semibold text-slate-100">{scenario.name}</h3>
            </div>
            <p className="text-sm text-slate-500">{scenario.description}</p>
          </div>
        ))}
      </div>
    </div>
  )
}
