export default function SettingsPage() {
  return (
    <div className="h-full">
      <div className="mb-6">
        <h1 className="text-3xl font-bold text-foreground">Settings</h1>
        <p className="text-muted-foreground mt-1">
          Configure controller preferences
        </p>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {/* System Settings */}
        <div className="bg-card rounded-xl border p-6">
          <h3 className="font-semibold text-lg mb-4">System</h3>

          <div className="space-y-4">
            {[
              { label: 'Server Address', value: 'localhost:7380', editable: false },
              { label: 'WebSocket URL', value: 'ws://localhost:7380/api/v1/ws', editable: false },
              { label: 'API Version', value: 'v1.0.0', editable: false },
              { label: 'Theme', value: 'Dark', editable: true },
              { label: 'Auto-connect', value: 'Enabled', editable: true },
            ].map((setting, i) => (
              <div key={i} className="flex items-center justify-between py-3 border-b border-border/50 last:border-0">
                <div>
                  <div className="font-medium">{setting.label}</div>
                  {setting.editable && (
                    <div className="text-xs text-muted-foreground">
                      Click to change
                    </div>
                  )}
                </div>
                <div className="flex items-center gap-3">
                  <div className={`text-sm font-mono px-2 py-1 rounded ${setting.editable ? 'bg-muted' : ''}`}>
                    {setting.value}
                  </div>
                  {setting.editable && (
                    <button className="p-2 text-muted-foreground hover:text-foreground transition-colors">
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
                        <path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z" />
                      </svg>
                    </button>
                  )}
                </div>
              </div>
            ))}
          </div>
        </div>

        {/* Network Settings */}
        <div className="bg-card rounded-xl border p-6">
          <h3 className="font-semibold text-lg mb-4">Network</h3>

          <div className="space-y-4">
            {[
              { label: 'Default Port', value: '7380', desc: 'API server port' },
              { label: 'Stream Port', value: '8080', desc: 'Video stream port' },
              { label: 'VNC Port', value: '5900', desc: 'Remote desktop port' },
              { label: 'Max Connections', value: '100', desc: 'Simultaneous clients' },
            ].map((setting, i) => (
              <div key={i} className="flex items-center justify-between py-3 border-b border-border/50 last:border-0">
                <div>
                  <div className="font-medium">{setting.label}</div>
                  <div className="text-xs text-muted-foreground">{setting.desc}</div>
                </div>
                <div className="flex items-center gap-3">
                  <div className="text-sm font-mono bg-muted px-2 py-1 rounded">
                    {setting.value}
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>

        {/* User Preferences */}
        <div className="bg-card rounded-xl border p-6 lg:col-span-2">
          <h3 className="font-semibold text-lg mb-4">User Preferences</h3>

          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            {[
              { label: 'Show node details on hover', default: true },
              { label: 'Auto-refresh status', default: true },
              { label: 'Enable touch gestures', default: true },
              { label: 'Show video thumbnails', default: true },
            ].map((pref, i) => (
              <label key={i} className="flex items-center justify-between p-4 rounded-lg border border-border hover:bg-muted/30 cursor-pointer transition-colors">
                <span className="font-medium">{pref.label}</span>
                <div className="relative">
                  <input type="checkbox" className="sr-only peer" defaultChecked={pref.default} />
                  <div className="w-11 h-6 bg-muted rounded-full peer peer-checked:bg-emerald-500 peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:border-gray-300 after:border after:rounded-full after:h-5 after:w-5 after:transition-all"></div>
                </div>
              </label>
            ))}
          </div>
        </div>

        {/* Action Buttons */}
        <div className="bg-card rounded-xl border p-6 lg:col-span-2">
          <div className="flex flex-wrap gap-3">
            <button className="px-4 py-2 text-sm font-medium bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors">
              Save Changes
            </button>
            <button className="px-4 py-2 text-sm font-medium bg-secondary text-secondary-foreground rounded-lg hover:bg-secondary/90 transition-colors">
              Reset Defaults
            </button>
            <button className="px-4 py-2 text-sm font-medium text-destructive hover:bg-destructive/10 rounded-lg transition-colors">
              Reset to Factory Settings
            </button>
          </div>
        </div>
      </div>
    </div>
  )
}
