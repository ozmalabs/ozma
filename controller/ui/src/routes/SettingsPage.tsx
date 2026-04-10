import { useState } from 'react'

export default function SettingsPage() {
  const [theme, setTheme] = useState<'dark' | 'light'>('dark')

  return (
    <div className="space-y-6 max-w-2xl">
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Settings</h1>
        <p className="text-slate-400 text-sm mt-1">
          Configure your controller preferences
        </p>
      </div>

      <div className="bg-slate-900 rounded-xl border border-slate-800 overflow-hidden">
        <div className="p-5 border-b border-slate-800">
          <h3 className="font-semibold text-slate-100">Appearance</h3>
          <p className="text-sm text-slate-500 mt-1">Customize the interface look and feel</p>
        </div>
        <div className="p-5 space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium text-slate-200">Theme</p>
              <p className="text-sm text-slate-500">Choose your preferred color theme</p>
            </div>
            <button
              onClick={() => setTheme(theme === 'dark' ? 'light' : 'dark')}
              className={`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${
                theme === 'dark' ? 'bg-emerald-500' : 'bg-slate-700'
              }`}
            >
              <span className={`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${theme === 'dark' ? 'translate-x-6' : 'translate-x-1'}`} />
            </button>
          </div>
        </div>
      </div>

      <div className="bg-slate-900 rounded-xl border border-slate-800 overflow-hidden">
        <div className="p-5 border-b border-slate-800">
          <h3 className="font-semibold text-slate-100">Controller</h3>
          <p className="text-sm text-slate-500 mt-1">System configuration</p>
        </div>
        <div className="p-5 space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium text-slate-200">Auto-start nodes</p>
              <p className="text-sm text-slate-500">Automatically activate nodes on connection</p>
            </div>
            <div className="relative inline-flex h-6 w-11 items-center rounded-full bg-slate-700">
              <span className="inline-block h-4 w-4 transform rounded-full bg-white translate-x-6" />
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
