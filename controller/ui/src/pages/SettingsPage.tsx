import { useState } from 'react'
import { useAuth } from '../store/useAuthStore'

export default function SettingsPage() {
  const { user, logout, error } = useAuth()
  const [settings, setSettings] = useState({
    autoRefresh: true,
    refreshInterval: 30,
    notifications: true,
    theme: 'dark',
  })

  const handleSettingChange = (key: string, value: unknown) => {
    setSettings((prev) => ({
      ...prev,
      [key]: value,
    }))
  }

  return (
    <div className="max-w-3xl mx-auto">
      <div className="mb-6">
        <h1 className="text-2xl font-bold">Settings</h1>
        <p className="text-muted-foreground">Configure your controller preferences</p>
      </div>

      {error && (
        <div className="mb-6 p-4 rounded-lg bg-destructive/10 border border-destructive/20">
          <p className="text-sm text-destructive">{error}</p>
        </div>
      )}

      <div className="bg-card rounded-xl border overflow-hidden">
        {/* User Settings */}
        <div className="p-6 border-b">
          <h2 className="text-lg font-semibold mb-4">User Information</h2>
          {user ? (
            <div className="space-y-4">
              <div className="flex items-center gap-4">
                <div className="w-16 h-16 bg-primary/10 rounded-full flex items-center justify-center text-primary">
                  <svg
                    xmlns="http://www.w3.org/2000/svg"
                    width="32"
                    height="32"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  >
                    <path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2" />
                    <circle cx="12" cy="7" r="4" />
                  </svg>
                </div>
                <div>
                  <h3 className="font-semibold text-lg">{user.username}</h3>
                  <p className="text-muted-foreground">{user.email}</p>
                  <div className="flex flex-wrap gap-2 mt-2">
                    {user.roles.map((role) => (
                      <span
                        key={role}
                        className="px-2 py-1 text-xs font-medium bg-secondary text-foreground rounded-full"
                      >
                        {role}
                      </span>
                    ))}
                  </div>
                </div>
              </div>
            </div>
          ) : (
            <p className="text-muted-foreground">Not authenticated</p>
          )}
        </div>

        {/* Application Settings */}
        <div className="p-6">
          <h2 className="text-lg font-semibold mb-4">Application Settings</h2>
          <div className="space-y-4">
            <div className="flex items-center justify-between py-2">
              <div>
                <h4 className="font-medium">Auto Refresh</h4>
                <p className="text-sm text-muted-foreground">
                  Automatically refresh the node list
                </p>
              </div>
              <label className="relative inline-flex items-center cursor-pointer">
                <input
                  type="checkbox"
                  checked={settings.autoRefresh}
                  onChange={(e) => handleSettingChange('autoRefresh', e.target.checked)}
                  className="sr-only peer"
                />
                <div className="w-11 h-6 bg-gray-700 peer-focus:outline-none rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary"></div>
              </label>
            </div>

            <div className="flex items-center justify-between py-2">
              <div>
                <h4 className="font-medium">Notifications</h4>
                <p className="text-sm text-muted-foreground">
                  Enable desktop notifications
                </p>
              </div>
              <label className="relative inline-flex items-center cursor-pointer">
                <input
                  type="checkbox"
                  checked={settings.notifications}
                  onChange={(e) => handleSettingChange('notifications', e.target.checked)}
                  className="sr-only peer"
                />
                <div className="w-11 h-6 bg-gray-700 peer-focus:outline-none rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-primary"></div>
              </label>
            </div>

            <div className="py-2">
              <label className="block text-sm font-medium mb-1">Refresh Interval</label>
              <div className="flex items-center gap-4">
                <input
                  type="range"
                  min="10"
                  max="60"
                  step="5"
                  value={settings.refreshInterval}
                  onChange={(e) => handleSettingChange('refreshInterval', parseInt(e.target.value))}
                  className="w-full h-2 bg-gray-700 rounded-lg appearance-none cursor-pointer"
                />
                <span className="text-sm font-mono text-muted-foreground">{settings.refreshInterval}s</span>
              </div>
            </div>
          </div>
        </div>

        {/* Theme Settings */}
        <div className="p-6">
          <h2 className="text-lg font-semibold mb-4">Appearance</h2>
          <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
            <button
              onClick={() => handleSettingChange('theme', 'dark')}
              className={`p-4 rounded-lg border transition-colors ${
                settings.theme === 'dark'
                  ? 'bg-primary/10 border-primary'
                  : 'bg-secondary border-transparent hover:bg-secondary/50'
              }`}
            >
              <div className="flex items-center gap-2 mb-2">
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
                  <path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" />
                </svg>
                <span className="font-medium">Dark</span>
              </div>
              <p className="text-xs text-muted-foreground">Default theme</p>
            </button>

            <button
              onClick={() => handleSettingChange('theme', 'light')}
              className={`p-4 rounded-lg border transition-colors ${
                settings.theme === 'light'
                  ? 'bg-primary/10 border-primary'
                  : 'bg-secondary border-transparent hover:bg-secondary/50'
              }`}
            >
              <div className="flex items-center gap-2 mb-2">
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
                  <circle cx="12" cy="12" r="5" />
                  <path d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42" />
                </svg>
                <span className="font-medium">Light</span>
              </div>
              <p className="text-xs text-muted-foreground">Light mode theme</p>
            </button>
          </div>
        </div>

        {/* Account Actions */}
        <div className="p-6 border-t">
          <h2 className="text-lg font-semibold mb-4">Account</h2>
          <button
            onClick={logout}
            className="px-4 py-2 bg-destructive text-destructive-foreground rounded-lg hover:bg-destructive/90 transition-colors flex items-center gap-2"
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
              <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" />
              <polyline points="16 17 21 12 16 7" />
              <line x1="21" x2="9" y1="12" y2="12" />
            </svg>
            Sign Out
          </button>
        </div>
      </div>
    </div>
  )
}
