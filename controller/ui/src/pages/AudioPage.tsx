export default function AudioPage() {
  return (
    <div className="h-full">
      <div className="mb-6">
        <h1 className="text-3xl font-bold text-foreground">Audio</h1>
        <p className="text-muted-foreground mt-1">
          Configure audio routing and settings
        </p>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {/* Audio Devices */}
        <div className="bg-card rounded-xl border p-6">
          <div className="flex items-center justify-between mb-4">
            <h3 className="font-semibold text-lg">Audio Outputs</h3>
            <button className="px-3 py-1.5 text-sm bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors">
              Add Device
            </button>
          </div>

          <div className="space-y-3">
            {[
              { name: 'Main Monitor', type: 'HDMI', status: 'active', volume: 75 },
              { name: 'Backup Speaker', type: 'Analog', status: 'standby', volume: 60 },
              { name: 'Zone A Output', type: 'Network', status: 'active', volume: 85 },
            ].map((device, i) => (
              <div key={i} className="flex items-center gap-4 p-3 bg-muted/30 rounded-lg">
                <div className={`h-2 w-2 rounded-full ${device.status === 'active' ? 'bg-emerald-500' : 'bg-amber-500'}`} />
                <div className="flex-1">
                  <div className="font-medium">{device.name}</div>
                  <div className="text-xs text-muted-foreground">{device.type}</div>
                </div>
                <div className="flex items-center gap-2">
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
                    className="text-muted-foreground"
                  >
                    <polygon points="11 5 6 9 2 12 2 12 2 12 2 12 6 15 11 19 11 5" />
                  </svg>
                  <div className="w-24 h-2 bg-muted rounded-full overflow-hidden">
                    <div className="h-full bg-primary" style={{ width: `${device.volume}%` }} />
                  </div>
                  <span className="text-xs font-mono w-8 text-right">{device.volume}%</span>
                </div>
              </div>
            ))}
          </div>
        </div>

        {/* Audio Settings */}
        <div className="bg-card rounded-xl border p-6">
          <h3 className="font-semibold text-lg mb-4">Global Settings</h3>

          <div className="space-y-4">
            {[
              { label: 'Audio Latency', value: 'Low (10ms)', desc: 'Optimized for live streaming' },
              { label: 'Sample Rate', value: '48 kHz', desc: 'Professional audio quality' },
              { label: 'Bit Depth', value: '24-bit', desc: 'High dynamic range' },
              { label: 'Default Output', value: 'Main Monitor', desc: 'Primary audio output device' },
            ].map((setting, i) => (
              <div key={i} className="flex items-center justify-between py-2 border-b border-border/50 last:border-0">
                <div>
                  <div className="font-medium">{setting.label}</div>
                  <div className="text-xs text-muted-foreground">{setting.desc}</div>
                </div>
                <div className="flex items-center gap-3">
                  <div className="text-sm font-mono bg-muted px-2 py-1 rounded">
                    {setting.value}
                  </div>
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
                      <path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" />
                      <path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z" />
                    </svg>
                  </button>
                </div>
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  )
}
