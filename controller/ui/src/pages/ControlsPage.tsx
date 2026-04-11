import { useState, useEffect } from 'react'
import { Link } from 'react-router-dom'

interface ControlBinding {
  action: string
  target: string | null
  value: any
}

interface Control {
  name: string
  value: any
  binding: ControlBinding | null
  lockout: boolean
}

interface DisplayControl {
  name: string
  value: string
  binding: string
}

interface ControlSurface {
  id: string
  type: 'midi' | 'streamdeck' | 'gamepad' | 'hotkeys' | 'virtual'
  device?: string
  controls: Record<string, Control>
  displays: Record<string, DisplayControl>
}

// Mock data for demo - will be replaced with actual API calls
const MOCK_SURFACES: ControlSurface[] = [
  {
    id: 'midi-xtouch',
    type: 'midi',
    device: 'X-Touch One',
    controls: {
      fader1: { name: 'fader1', value: 85, binding: { action: 'audio.volume', target: '@active', value: null }, lockout: false },
      button1: { name: 'button1', value: false, binding: { action: 'scenario.activate', target: 'matt-workstation', value: null }, lockout: false },
    },
    displays: {},
  },
  {
    id: 'streamdeck-main',
    type: 'streamdeck',
    device: 'Stream Deck Mini',
    controls: {
      key_0: { name: 'key_0', value: null, binding: { action: 'scenario.activate', target: 'streamdeck-main', value: 'home' }, lockout: false },
      key_1: { name: 'key_1', value: null, binding: { action: 'scenario.activate', target: 'streamdeck-main', value: 'studio' }, lockout: false },
      key_2: { name: 'key_2', value: null, binding: { action: 'scenario.activate', target: 'streamdeck-main', value: 'office' }, lockout: false },
      key_3: { name: 'key_3', value: null, binding: { action: 'scenario.activate', target: 'streamdeck-main', value: 'post' }, lockout: false },
      key_4: { name: 'key_4', value: null, binding: { action: 'scenario.activate', target: 'streamdeck-main', value: 'stream' }, lockout: false },
      key_5: { name: 'key_5', value: null, binding: { action: 'audio.mute', target: '@active', value: null }, lockout: false },
    },
    displays: {},
  },
  {
    id: 'gamepad-xbox',
    type: 'gamepad',
    device: 'Xbox Wireless Controller',
    controls: {
      south: { name: 'south (A)', value: null, binding: { action: 'scenario.activate', target: 'gamepad-xbox', value: null }, lockout: false },
      lb: { name: 'lb', value: null, binding: { action: 'scenario.next', target: null, value: -1 }, lockout: false },
      rb: { name: 'rb', value: null, binding: { action: 'scenario.next', target: null, value: 1 }, lockout: false },
      guide: { name: 'guide', value: null, binding: { action: 'audio.mute', target: '@active', value: null }, lockout: false },
      rt_volume: { name: 'rt_volume', value: null, binding: { action: 'audio.volume', target: '@active', value: null }, lockout: false },
      dpad_up: { name: 'dpad_up', value: null, binding: { action: 'audio.volume_step', target: '@active', value: 0.05 }, lockout: false },
      dpad_down: { name: 'dpad_down', value: null, binding: { action: 'audio.volume_step', target: '@active', value: -0.05 }, lockout: false },
      dpad_left: { name: 'dpad_left', value: null, binding: { action: 'scenario.next', target: null, value: -1 }, lockout: false },
      dpad_right: { name: 'dpad_right', value: null, binding: { action: 'scenario.next', target: null, value: 1 }, lockout: false },
    },
    displays: {},
  },
]

export default function ControlsPage() {
  const [surfaces, setSurfaces] = useState<ControlSurface[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    fetchControls()
  }, [])

  async function fetchControls() {
    try {
      setLoading(true)
      // TODO: Replace with actual GraphQL/API call
      // const response = await fetch('/api/v1/controls')
      // const data = await response.json()
      setSurfaces(MOCK_SURFACES)
      setError(null)
    } catch (err) {
      setError('Failed to load control surfaces')
      console.error('Error fetching controls:', err)
    } finally {
      setLoading(false)
    }
  }

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
          <p className="text-muted-foreground">Loading control surfaces...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center max-w-md">
          <div className="text-destructive mb-4">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="48"
              height="48"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <circle cx="12" cy="12" r="10" />
              <line x1="15" x2="9" y1="9" y2="15" />
              <line x1="9" x2="15" y1="9" y2="15" />
            </svg>
          </div>
          <h3 className="text-xl font-semibold mb-2">Failed to load controls</h3>
          <p className="text-muted-foreground mb-6">{error}</p>
          <button
            onClick={fetchControls}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  const midiSurfaces = surfaces.filter(s => s.type === 'midi')
  const streamdeckSurfaces = surfaces.filter(s => s.type === 'streamdeck')
  const gamepadSurfaces = surfaces.filter(s => s.type === 'gamepad')

  return (
    <div className="max-w-6xl mx-auto">
      <div className="flex justify-between items-center mb-6">
        <div>
          <h2 className="text-2xl font-bold">Control Surfaces</h2>
          <p className="text-muted-foreground">MIDI controllers, Stream Decks, and gamepads</p>
        </div>
        <div className="flex gap-2">
          <button
            onClick={fetchControls}
            className="px-4 py-2 bg-secondary text-foreground rounded-lg hover:bg-secondary/80 transition-colors flex items-center gap-2"
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
              <path d="M21 12a9 9 0 0 0-9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
              <path d="M3 3v5h5" />
              <path d="M3 12a9 9 0 0 0 9 9 9.75 9.75 0 0 0 6.74-2.74L21 16" />
              <path d="M16 16h5v5" />
            </svg>
            Refresh
          </button>
        </div>
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
        {midiSurfaces.length > 0 && (
          <section className="md:col-span-2">
            <h3 className="text-lg font-semibold mb-4 flex items-center gap-2">
              <span className="w-2 h-2 rounded-full bg-blue-500"></span>
              MIDI Controllers ({midiSurfaces.length})
            </h3>
            {midiSurfaces.map((surface) => (
              <SurfaceCard
                key={surface.id}
                surface={surface}
                iconType="midi"
              />
            ))}
          </section>
        )}

        {streamdeckSurfaces.length > 0 && (
          <section className="md:col-span-2">
            <h3 className="text-lg font-semibold mb-4 flex items-center gap-2">
              <span className="w-2 h-2 rounded-full bg-purple-500"></span>
              Stream Decks ({streamdeckSurfaces.length})
            </h3>
            {streamdeckSurfaces.map((surface) => (
              <SurfaceCard
                key={surface.id}
                surface={surface}
                iconType="streamdeck"
              />
            ))}
          </section>
        )}

        {gamepadSurfaces.length > 0 && (
          <section className="md:col-span-2">
            <h3 className="text-lg font-semibold mb-4 flex items-center gap-2">
              <span className="w-2 h-2 rounded-full bg-green-500"></span>
              Gamepads ({gamepadSurfaces.length})
            </h3>
            {gamepadSurfaces.map((surface) => (
              <SurfaceCard
                key={surface.id}
                surface={surface}
                iconType="gamepad"
              />
            ))}
          </section>
        )}

        {surfaces.length === 0 && (
          <div className="text-center py-12 border-2 border-dashed border-border rounded-xl col-span-full">
            <div className="text-muted-foreground mb-4">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="48"
                height="48"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
                <path d="M12 12v6" />
                <path d="M8 12v6" />
                <path d="M16 12v6" />
              </svg>
            </div>
            <h3 className="text-lg font-semibold mb-2">No control surfaces found</h3>
            <p className="text-muted-foreground mb-6">
              Connect a MIDI controller, Stream Deck, or gamepad to get started.
            </p>
            <div className="flex justify-center gap-4">
              <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors">
                Learn More
              </button>
            </div>
          </div>
        )}
      </div>

      <div className="mt-8 p-6 bg-card rounded-xl border">
        <h3 className="text-lg font-semibold mb-4">About Control Surfaces</h3>
        <div className="grid grid-cols-1 md:grid-cols-3 gap-6 text-sm text-muted-foreground">
          <div>
            <h4 className="font-medium text-foreground mb-2">MIDI Controllers</h4>
            <p>
              Connect USB MIDI controllers like Behringer X-Touch, Native Instruments Komplete, or any standard MIDI device.
            </p>
          </div>
          <div>
            <h4 className="font-medium text-foreground mb-2">Stream Decks</h4>
            <p>
              Elgato Stream Decks show scenario names and colors on their LCD screens. Press a key to activate a scenario.
            </p>
          </div>
          <div>
            <h4 className="font-medium text-foreground mb-2">Gamepads</h4>
            <p>
              Xbox, PlayStation, and generic gamepads work out of the box with default bindings for scenario and audio control.
            </p>
          </div>
        </div>
      </div>
    </div>
  )
}

interface SurfaceCardProps {
  surface: ControlSurface
  iconType: 'midi' | 'streamdeck' | 'gamepad'
}

function SurfaceCard({ surface, iconType }: SurfaceCardProps) {
  const getIcon = () => {
    switch (iconType) {
      case 'midi':
        return (
          <svg
            xmlns="http://www.w3.org/2000/svg"
            width="24"
            height="24"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M9 18V5l12-2v13" />
            <circle cx="6" cy="18" r="3" />
            <circle cx="18" cy="16" r="3" />
            <path d="M9 11v2" />
            <path d="M9 17v2" />
            <path d="M18 12v2" />
            <path d="M18 14v2" />
          </svg>
        )
      case 'streamdeck':
        return (
          <svg
            xmlns="http://www.w3.org/2000/svg"
            width="24"
            height="24"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <rect width="20" height="16" x="2" y="4" rx="2" />
            <path d="M6 8h.01" />
            <path d="M10 8h.01" />
            <path d="M14 8h.01" />
            <path d="M18 8h.01" />
            <path d="M6 12h.01" />
            <path d="M10 12h.01" />
            <path d="M14 12h.01" />
            <path d="M18 12h.01" />
            <path d="M6 16h.01" />
            <path d="M10 16h.01" />
            <path d="M14 16h.01" />
          </svg>
        )
      case 'gamepad':
        return (
          <svg
            xmlns="http://www.w3.org/2000/svg"
            width="24"
            height="24"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <line x1="6" x2="10" y1="12" y2="12" />
            <line x1="8" x2="8" y1="10" y2="14" />
            <line x1="15" x2="15" y1="13" y2="13" />
            <line x1="15" x2="17" y1="11" y2="11" />
            <line x1="15" x2="17" y1="15" y2="15" />
            <line x1="10" x2="12" y1="16" y2="16" />
            <line x1="11" x2="11" y1="15" y2="17" />
            <rect width="20" height="12" x="2" y="6" rx="2" />
            <circle cx="6" cy="10" r="1" />
            <circle cx="6" cy="14" r="1" />
          </svg>
        )
      default:
        return null
    }
  }

  return (
    <div className="bg-card rounded-xl border p-5 hover:border-primary/50 transition-all">
      <div className="flex justify-between items-start mb-4">
        <div className="flex items-center gap-3">
          <div className="p-2 bg-primary/10 rounded-lg text-primary">
            {getIcon()}
          </div>
          <div>
            <h3 className="font-semibold text-lg">{surface.device || surface.id}</h3>
            <p className="text-sm text-muted-foreground">
              {surface.type.charAt(0).toUpperCase() + surface.type.slice(1)}
            </p>
          </div>
        </div>
        <div className="flex gap-2">
          <span className="px-2 py-1 text-xs font-medium bg-secondary text-foreground rounded-full">
            {Object.keys(surface.controls).length} controls
          </span>
        </div>
      </div>

      <div className="space-y-2">
        <h4 className="text-sm font-medium text-muted-foreground">Controls</h4>
        <div className="grid grid-cols-2 md:grid-cols-3 gap-2">
          {Object.entries(surface.controls).slice(0, 6).map(([name, control]) => (
            <div
              key={name}
              className="px-2 py-1.5 bg-secondary/50 rounded text-xs"
              title={name}
            >
              {control.binding ? (
                <div className="flex flex-col">
                  <span className="font-medium truncate">{control.name}</span>
                  <span className="text-[10px] text-muted-foreground truncate">
                    {control.binding.action}
                  </span>
                </div>
              ) : (
                <span className="text-muted-foreground">{control.name}</span>
              )}
            </div>
          ))}
          {Object.keys(surface.controls).length > 6 && (
            <div className="px-2 py-1.5 bg-secondary/50 rounded text-xs text-muted-foreground">
              +{Object.keys(surface.controls).length - 6} more
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
