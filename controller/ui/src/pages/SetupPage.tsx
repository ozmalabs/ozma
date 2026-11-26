import { useEffect, useMemo, useState } from 'react'
import { apiFetch } from '../api/client'

type GraphDevice = {
  id: string
  name?: string
  type?: string
  ports?: Array<{ direction?: string; media_type?: string }>
}

type StreamIntent = {
  media_type: string
  required?: boolean
}

type RoutingIntent = {
  name: string
  description?: string
  streams?: StreamIntent[]
}

type Feasibility = {
  source: string
  destination: string
  intent: string
  feasible: Record<string, boolean>
}

type RouteExplanation = {
  streams?: Array<{ media_type?: string; pipelines?: unknown[] }>
}

type PermissionName = 'view' | 'control' | 'audio' | 'mic' | 'clipboard_read' | 'clipboard_write'

type ApertureReceipt = {
  requested?: { permissions?: string[] }
  effective?: { permissions?: string[] }
  limiting?: Array<{ dimension?: string; frame?: string; provenance?: string }>
  residual?: Array<{ dimension?: string }>
  recourse?: string | null
  policy_version?: string
  input_hash?: string
}

type SessionEnvelope = {
  session?: { id?: string; state?: string; permissions?: string[] }
  aperture?: ApertureReceipt | null
  quality?: {
    requested?: { max_fps?: number; max_bitrate_kbps?: number }
    effective?: { max_fps?: number; max_bitrate_kbps?: number }
    limiting?: Array<{ source?: string; max_fps?: number; max_bitrate_kbps?: number }>
    policy_receipt?: { policy_version?: string; input_hash?: string }
  } | null
}

type SavedSetup = {
  id: string
  name: string
  setup_profile?: unknown
}

type SetupProfile = {
  version: 1
  intent: 'control' | 'observe' | 'desktop'
  goal_id?: 'control' | 'observe' | 'desktop' | 'custom' | null
  source_endpoint_id?: string
  target_node_id: string
  permissions: {
    view: true
    control: boolean
    audio: boolean
    microphone: boolean
    clipboard: boolean
  }
  session_options: {
    ttl_seconds: number
    consent: 'always'
    indicate: true
  }
  quality: { max_fps: 15 | 24 | 30; max_bitrate_kbps: 1000 | 2000 | 4000 }
  ui_aperture_level: 'simple' | 'guided' | 'fine' | 'expert'
}

const GOALS = [
  {
    id: 'control',
    title: 'Control another computer',
    description: 'Use this Ozma controller’s keyboard and mouse on a connected computer.',
    intent: 'desktop',
    defaultControl: true,
    defaultAudio: false,
    icon: '⌨',
  },
  {
    id: 'observe',
    title: 'View a computer without controlling it',
    description: 'Watch a connected computer without sending keyboard or mouse input.',
    intent: 'observe',
    defaultControl: false,
    defaultAudio: false,
    icon: '◉',
  },
  {
    id: 'desktop',
    title: 'Use a computer as a full remote workspace',
    description: 'Ask Ozma to find the available picture, sound, and input paths.',
    intent: 'desktop',
    defaultControl: true,
    defaultAudio: true,
    icon: '▣',
  },
  {
    id: 'custom',
    title: 'Create a custom setup',
    description: 'Choose the capabilities and access duration that fit this connection.',
    intent: 'desktop',
    defaultControl: false,
    defaultAudio: false,
    icon: '⚙',
  },
]

const MEDIA_LABELS: Record<string, string> = {
  hid: 'Keyboard and mouse',
  video: 'Picture',
  screen: 'Screen',
  audio: 'Sound',
  control: 'Control connection',
  data: 'Data',
  rgb: 'Lighting',
}

function displayName(device: GraphDevice): string {
  const name = device.name?.trim()
  if (name) return name
  if (device.type === 'controller') return 'This Ozma controller'
  if (device.type === 'node') return 'Connected computer'
  if (device.type === 'target') return 'Computer connected to an Ozma node'
  return device.type ? `${device.type} device` : 'Ozma device'
}

function responseError(data: unknown, fallback: string): string {
  if (data && typeof data === 'object') {
    const value = data as { detail?: unknown; error?: unknown; message?: unknown }
    for (const candidate of [value.detail, value.error, value.message]) {
      if (typeof candidate === 'string' && candidate.trim()) return candidate
    }
  }
  return fallback
}

const PERMISSION_LABELS: Record<PermissionName, string> = {
  view: 'View the computer',
  control: 'Keyboard and mouse',
  audio: 'Listen to audio',
  mic: 'Send microphone audio',
  clipboard_read: 'Read clipboard',
  clipboard_write: 'Write clipboard',
}

function sessionEnvelope(data: unknown): SessionEnvelope {
  if (!data || typeof data !== 'object') return {}
  const value = data as SessionEnvelope
  return value.session ? value : { session: data as SessionEnvelope['session'] }
}

function permissionNames(value: unknown): string[] | null {
  if (!value || typeof value !== 'object') return null
  const permissions = (value as { permissions?: unknown }).permissions
  return Array.isArray(permissions) && permissions.every(item => typeof item === 'string')
    ? permissions
    : null
}

function friendlyState(state: string): string {
  return state.split('_').map(part => part ? part[0].toUpperCase() + part.slice(1) : part).join(' ')
}

function decodeSetupProfile(value: unknown): SetupProfile {
  const unsupported = () => new Error('This saved setup is incomplete or uses an unsupported format. Create a new setup and save it again.')
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw unsupported()
  const profile = value as Record<string, unknown>
  if (!Object.keys(profile).every(key => [
    'version', 'intent', 'goal_id', 'source_endpoint_id', 'target_node_id', 'permissions', 'session_options', 'quality', 'ui_aperture_level',
  ].includes(key))) throw unsupported()
  if (profile.version !== 1) throw unsupported()
  if (!['control', 'observe', 'desktop'].includes(String(profile.intent))) throw unsupported()
  if (profile.goal_id !== undefined && profile.goal_id !== null && !['control', 'observe', 'desktop', 'custom'].includes(String(profile.goal_id))) throw unsupported()
  if (typeof profile.target_node_id !== 'string' || !profile.target_node_id.trim() ||
      profile.target_node_id !== profile.target_node_id.trim() || profile.target_node_id.length > 255) throw unsupported()
  if (profile.source_endpoint_id !== undefined &&
      (typeof profile.source_endpoint_id !== 'string' || !profile.source_endpoint_id.trim() ||
       profile.source_endpoint_id !== profile.source_endpoint_id.trim() || profile.source_endpoint_id.length > 255)) throw unsupported()
  if (!['simple', 'guided', 'fine', 'expert'].includes(String(profile.ui_aperture_level))) throw unsupported()

  const quality = profile.quality === undefined ? {} : profile.quality
  if (!quality || typeof quality !== 'object' || Array.isArray(quality)) throw unsupported()
  const qualityValues = quality as Record<string, unknown>
  const maxFps = qualityValues.max_fps === undefined ? 30 : qualityValues.max_fps
  const maxBitrateKbps = qualityValues.max_bitrate_kbps === undefined ? 2000 : qualityValues.max_bitrate_kbps
  if (!Object.keys(qualityValues).every(key => ['max_fps', 'max_bitrate_kbps'].includes(key)) ||
      typeof maxFps !== 'number' || ![15, 24, 30].includes(maxFps) ||
      typeof maxBitrateKbps !== 'number' || ![1000, 2000, 4000].includes(maxBitrateKbps)) throw unsupported()
  const migratedQuality = { max_fps: maxFps as 15 | 24 | 30, max_bitrate_kbps: maxBitrateKbps as 1000 | 2000 | 4000 }

  const permissions = profile.permissions
  if (!permissions || typeof permissions !== 'object' || Array.isArray(permissions)) throw unsupported()
  const permissionValues = permissions as Record<string, unknown>
  if (!Object.keys(permissionValues).every(key => ['view', 'control', 'audio', 'microphone', 'clipboard'].includes(key)) ||
      permissionValues.view !== true ||
      !['control', 'audio', 'microphone', 'clipboard'].every(key => typeof permissionValues[key] === 'boolean') ||
      (permissionValues.microphone === true && permissionValues.audio !== true)) throw unsupported()

  const options = profile.session_options
  if (!options || typeof options !== 'object' || Array.isArray(options)) throw unsupported()
  const sessionOptions = options as Record<string, unknown>
  if (!Object.keys(sessionOptions).every(key => ['ttl_seconds', 'consent', 'indicate'].includes(key)) ||
      typeof sessionOptions.ttl_seconds !== 'number' || !Number.isInteger(sessionOptions.ttl_seconds) ||
      sessionOptions.ttl_seconds < 1 || sessionOptions.ttl_seconds > 86400 ||
      sessionOptions.consent !== 'always' || sessionOptions.indicate !== true) throw unsupported()

  return { ...profile, quality: migratedQuality } as SetupProfile
}

export default function SetupPage() {
  const [devices, setDevices] = useState<GraphDevice[]>([])
  const [intents, setIntents] = useState<Record<string, RoutingIntent>>({})
  const [selectedGoal, setSelectedGoal] = useState('control')
  const [showInboundInstructions, setShowInboundInstructions] = useState(false)
  const [source, setSource] = useState('controller')
  const [destination, setDestination] = useState('')
  const [feasibility, setFeasibility] = useState<Feasibility | null>(null)
  const [routeExplanation, setRouteExplanation] = useState<RouteExplanation | null>(null)
  const [detailLevel, setDetailLevel] = useState<'simple' | 'guided' | 'fine' | 'expert'>('simple')
  const [control, setControl] = useState(true)
  const [audio, setAudio] = useState(false)
  const [microphone, setMicrophone] = useState(false)
  const [clipboard, setClipboard] = useState(false)
  const [sessionLength, setSessionLength] = useState(3600)
  const [maxFps, setMaxFps] = useState<15 | 24 | 30>(30)
  const [maxBitrateKbps, setMaxBitrateKbps] = useState<1000 | 2000 | 4000>(2000)
  const [savedSetups, setSavedSetups] = useState<SavedSetup[]>([])
  const [savedSetupId, setSavedSetupId] = useState('')
  const [setupName, setSetupName] = useState('My setup')
  const [profileMessage, setProfileMessage] = useState<string | null>(null)
  const [savingProfile, setSavingProfile] = useState(false)
  const [session, setSession] = useState<SessionEnvelope | null>(null)
  const [refreshingSession, setRefreshingSession] = useState(false)
  const [startingSession, setStartingSession] = useState(false)
  const [stoppingSession, setStoppingSession] = useState(false)
  const [sessionMessage, setSessionMessage] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [checking, setChecking] = useState(false)
  const [requesting, setRequesting] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    async function load() {
      setLoading(true)
      setError(null)
      try {
        const [deviceResponse, intentResponse, scenarioResponse] = await Promise.all([
          apiFetch('/api/v1/graph/devices'),
          apiFetch('/api/v1/routing/intents'),
          apiFetch('/api/v1/scenarios'),
        ])
        const deviceData = await deviceResponse.json().catch(() => ({}))
        const intentData = await intentResponse.json().catch(() => ({}))
        const scenarioData = await scenarioResponse.json().catch(() => ({}))
        if (!deviceResponse.ok) throw new Error(responseError(deviceData, `Could not load Ozma devices (${deviceResponse.status}).`))
        if (!intentResponse.ok) throw new Error(responseError(intentData, `Could not load setup options (${intentResponse.status}).`))
        if (!scenarioResponse.ok) throw new Error(responseError(scenarioData, `Could not load saved setups (${scenarioResponse.status}).`))
        if (cancelled) return
        const loadedDevices: GraphDevice[] = Array.isArray(deviceData.devices) ? deviceData.devices : []
        setDevices(loadedDevices)
        setIntents(intentData.intents && typeof intentData.intents === 'object' ? intentData.intents : {})
        const loadedSetups: SavedSetup[] = Array.isArray(scenarioData.scenarios) ? scenarioData.scenarios : []
        setSavedSetups(loadedSetups)

        const targets = loadedDevices.filter(device => device.type === 'node')
        if (targets.length > 0) setDestination(targets[0].id)

        const lastSetupId = localStorage.getItem('ozma_last_setup_profile')
        if (lastSetupId) {
          if (loadedSetups.some(item => item.id === lastSetupId && item.setup_profile)) {
            await loadSetup(lastSetupId)
          } else {
            localStorage.removeItem('ozma_last_setup_profile')
          }
        }
      } catch (loadError) {
        if (!cancelled) setError(loadError instanceof Error ? loadError.message : 'Could not load setup options.')
      } finally {
        if (!cancelled) setLoading(false)
      }
    }
    void load()
    return () => { cancelled = true }
  }, [])

  const goal = GOALS.find(item => item.id === selectedGoal) ?? GOALS[0]
  const routingIntent = goal.id === 'custom'
    ? (control ? 'desktop' : 'observe')
    : goal.intent
  const intent = intents[routingIntent]
  const endpointOptions = useMemo(
    () => devices.filter(device => device.type === 'controller' || device.type === 'node' || device.type === 'target'),
    [devices],
  )
  const routeCheckSource = endpointOptions.find(device => device.id === source)
  const routeCheckSourceLabel = routeCheckSource
    ? displayName(routeCheckSource)
    : source === 'controller'
      ? 'this Ozma controller'
      : 'a saved starting computer that is no longer available'
  const requiredStreams = (intent?.streams ?? []).filter(stream => stream.required !== false)
  const optionalStreams = (intent?.streams ?? []).filter(stream => stream.required === false)
  const targetDevice = endpointOptions.find(device => device.id === destination)
  const targetNodeId = targetDevice?.type === 'node' && destination.startsWith('node:')
    ? destination.slice('node:'.length)
    : ''
  const targetReportsVideo = targetDevice?.ports?.some(port =>
    port.direction === 'source' && ['video', 'screen'].includes(port.media_type ?? ''),
  ) ?? false
  const requestedPermissions: PermissionName[] = [
    'view',
    ...(control ? ['control' as const] : []),
    ...(audio ? ['audio' as const] : []),
    ...(audio && microphone ? ['mic' as const] : []),
    ...(clipboard ? ['clipboard_read' as const, 'clipboard_write' as const] : []),
  ]
  const effectivePermissions = session?.aperture
    ? permissionNames(session.aperture.effective)
    : null
  const grantedPermissions = Array.isArray(session?.session?.permissions)
    ? session.session.permissions
    : null

  function chooseGoal(goalId: string) {
    const nextGoal = GOALS.find(item => item.id === goalId)
    setSelectedGoal(goalId)
    setControl(nextGoal?.defaultControl ?? false)
    setAudio(nextGoal?.defaultAudio ?? false)
    setMicrophone(false)
    setClipboard(false)
    setFeasibility(null)
    setRouteExplanation(null)
    setSession(null)
    setError(null)
  }

  function resetToRecommended() {
    setControl(goal.defaultControl)
    setAudio(goal.defaultAudio)
    setMicrophone(false)
    setClipboard(false)
    setSessionLength(3600)
    setMaxFps(30)
    setMaxBitrateKbps(2000)
    setFeasibility(null)
    setRouteExplanation(null)
    setSession(null)
    setSessionMessage(null)
    setError(null)
    setProfileMessage('Recommended choices restored. Save the setup if you want to keep them.')
  }

  async function checkSetup() {
    if (!source || !destination) {
      setError('Choose the Ozma devices this setup should connect.')
      return
    }
    setChecking(true)
    setError(null)
    setFeasibility(null)
    setRouteExplanation(null)
    try {
      const params = new URLSearchParams({ source, destination, intent: routingIntent })
      const response = await apiFetch(`/api/v1/routing/feasibility?${params}`)
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(responseError(data, `Ozma could not check this setup (${response.status}).`))
      setFeasibility(data as Feasibility)
      const explainResponse = await apiFetch(`/api/v1/routing/explain?${params}`)
      const explainData = await explainResponse.json().catch(() => ({}))
      if (explainResponse.ok) setRouteExplanation(explainData as RouteExplanation)
    } catch (checkError) {
      setError(checkError instanceof Error ? checkError.message : 'Ozma could not check this setup.')
    } finally {
      setChecking(false)
    }
  }

  async function saveSetup() {
    if (!targetNodeId) {
      setError('Choose a connected computer before saving this setup.')
      return
    }
    setSavingProfile(true)
    setError(null)
    setProfileMessage(null)
    const profile = {
      version: 1,
      intent: routingIntent,
      goal_id: goal.id,
      source_endpoint_id: source,
      target_node_id: targetNodeId,
      permissions: { view: true, control, audio, microphone: audio && microphone, clipboard },
      session_options: { ttl_seconds: sessionLength, consent: 'always', indicate: true },
      quality: { max_fps: maxFps, max_bitrate_kbps: maxBitrateKbps },
      ui_aperture_level: detailLevel,
    }
    const id = savedSetupId || `setup-${crypto.randomUUID()}`
    try {
      const response = await apiFetch(savedSetupId
        ? `/api/v1/scenarios/${encodeURIComponent(id)}/setup-profile`
        : `/api/v1/scenarios?scenario_id=${encodeURIComponent(id)}`, {
        method: savedSetupId ? 'PATCH' : 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(savedSetupId
          ? { name: setupName, setup_profile: profile }
          : { name: setupName, node_id: targetNodeId, setup_profile: profile }),
      })
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(responseError(data, `Could not save this setup (${response.status}).`))
      const saved = data as SavedSetup
      const savedId = saved.id || id
      setSavedSetupId(savedId)
      localStorage.setItem('ozma_last_setup_profile', savedId)
      const refreshed = await apiFetch('/api/v1/scenarios')
      const refreshedData = await refreshed.json().catch(() => ({}))
      if (refreshed.ok && Array.isArray(refreshedData.scenarios)) setSavedSetups(refreshedData.scenarios)
      setProfileMessage('Setup saved. Saving a profile does not request access or start a session.')
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : 'Could not save this setup.')
    } finally {
      setSavingProfile(false)
    }
  }

  async function loadSetup(id: string) {
    setSavedSetupId(id)
    setProfileMessage(null)
    if (!id) {
      localStorage.removeItem('ozma_last_setup_profile')
      return
    }
    setError(null)
    try {
      const response = await apiFetch(`/api/v1/scenarios/${encodeURIComponent(id)}`)
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(responseError(data, `Could not load this setup (${response.status}).`))
      const saved = data as SavedSetup
      const profile = decodeSetupProfile(saved.setup_profile)
      const permissions = profile.permissions
      setSetupName(saved.name || 'My setup')
      setSelectedGoal(profile.goal_id ?? (profile.intent === 'control' ? 'control' : profile.intent))
      setSource(typeof profile.source_endpoint_id === 'string' ? profile.source_endpoint_id : 'controller')
      setDestination(`node:${profile.target_node_id}`)
      setControl(permissions.control)
      setAudio(permissions.audio)
      setMicrophone(permissions.microphone)
      setClipboard(permissions.clipboard)
      setSessionLength(profile.session_options.ttl_seconds)
      setMaxFps(profile.quality.max_fps)
      setMaxBitrateKbps(profile.quality.max_bitrate_kbps)
      setDetailLevel(profile.ui_aperture_level)
      setFeasibility(null)
      setRouteExplanation(null)
      setSession(null)
      localStorage.setItem('ozma_last_setup_profile', id)
      setProfileMessage('Setup loaded. Review it and request access separately if you want a session.')
    } catch (loadError) {
      localStorage.removeItem('ozma_last_setup_profile')
      setError(loadError instanceof Error ? loadError.message : 'Could not load this setup.')
    }
  }

  async function requestAccess() {
    if (!targetNodeId) {
      setError('Choose a connected Ozma node as the computer to configure.')
      return
    }
    setRequesting(true)
    setError(null)
    setSession(null)
    setSessionMessage(null)
    try {
      const availabilityResponse = await apiFetch(
        `/api/v1/nodes/${encodeURIComponent(targetNodeId)}/session-availability`,
      )
      const availability = await availabilityResponse.json().catch(() => ({}))
      if (!availabilityResponse.ok) {
        throw new Error(responseError(availability, `Could not check the target computer (${availabilityResponse.status}).`))
      }
      if (availability.available !== true) {
        throw new Error(typeof availability.message === 'string'
          ? availability.message
          : 'This computer is not ready to accept a remote session yet.')
      }

      const response = await apiFetch(`/api/v1/nodes/${encodeURIComponent(targetNodeId)}/sessions`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          tracks: [{ track_id: 'screen0', kind: 'capture' }],
          quality: { max_fps: maxFps, max_bitrate_kbps: maxBitrateKbps },
          audio: { enabled: audio, mic: audio && microphone },
          input: {
            keyboard: control,
            pointer: control ? 'absolute' : 'none',
            gamepad: false,
            touch: false,
            pen: false,
            clipboard,
            owner_only: true,
          },
          privacy: { consent: 'always', indicate: true },
          transport: 'auto',
          ttl_seconds: sessionLength,
        }),
      })
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(responseError(data, `Ozma could not request this access (${response.status}).`))
      let envelope = sessionEnvelope(data)
      if (envelope.session?.id) {
        const inspect = await apiFetch(`/api/v1/nodes/${encodeURIComponent(targetNodeId)}/sessions/${encodeURIComponent(envelope.session.id)}`)
        const inspected = await inspect.json().catch(() => ({}))
        if (inspect.ok) envelope = sessionEnvelope(inspected)
      }
      const reportedFps = envelope.quality?.requested?.max_fps
      const effectiveFps = envelope.quality?.effective?.max_fps
      const reportedBitrate = envelope.quality?.requested?.max_bitrate_kbps
      const effectiveBitrate = envelope.quality?.effective?.max_bitrate_kbps
      if (reportedFps !== maxFps || !Number.isInteger(effectiveFps) || !effectiveFps || effectiveFps > maxFps ||
          reportedBitrate !== maxBitrateKbps || !Number.isInteger(effectiveBitrate) || !effectiveBitrate || effectiveBitrate > maxBitrateKbps) {
        let requestEnded = false
        if (envelope.session?.id) {
          try {
            const endResponse = await apiFetch(`/api/v1/nodes/${encodeURIComponent(targetNodeId)}/sessions/${encodeURIComponent(envelope.session.id)}/stop`, {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({}),
            })
            requestEnded = endResponse.ok
          } catch {
            requestEnded = false
          }
        }
        throw new Error(requestEnded
          ? 'The target computer did not confirm the requested video-quality settings. Ozma ended this request; update the target node before requesting access again.'
          : 'The target computer did not confirm the requested video-quality settings, and Ozma could not confirm that the request ended. If it remains pending, deny it from the target tray, then update the target node before trying again.')
      }
      setSession(envelope)
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : 'Ozma could not request this access.')
    } finally {
      setRequesting(false)
    }
  }

  async function refreshSession() {
    const id = session?.session?.id
    if (!targetNodeId || !id) return
    setRefreshingSession(true)
    setError(null)
    setSessionMessage(null)
    try {
      const response = await apiFetch(`/api/v1/nodes/${encodeURIComponent(targetNodeId)}/sessions/${encodeURIComponent(id)}`)
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(responseError(data, `Could not refresh this request (${response.status}).`))
      setSession(sessionEnvelope(data))
      setSessionMessage('Latest session state received from the target computer.')
    } catch (refreshError) {
      setError(refreshError instanceof Error ? refreshError.message : 'Could not refresh the session request.')
    } finally {
      setRefreshingSession(false)
    }
  }

  async function startSession() {
    const id = session?.session?.id
    if (!targetNodeId || !id) return
    setStartingSession(true)
    setError(null)
    setSessionMessage(null)
    try {
      const response = await apiFetch(`/api/v1/nodes/${encodeURIComponent(targetNodeId)}/sessions/${encodeURIComponent(id)}/start`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({}),
      })
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(responseError(data, `Could not start this session (${response.status}).`))
      const updated = sessionEnvelope(data)
      setSession(updated)
      const state = updated.session?.state
      setSessionMessage(state
        ? `The target approved the request. Session state: ${friendlyState(state)}. Media and input connection are not confirmed by this status.`
        : 'The target approved the request. Media and input connection are not confirmed by this response.')
    } catch (startError) {
      setError(startError instanceof Error ? startError.message : 'Could not start the session.')
    } finally {
      setStartingSession(false)
    }
  }

  async function stopSession() {
    const id = session?.session?.id
    if (!targetNodeId || !id) return
    setStoppingSession(true)
    setError(null)
    setSessionMessage(null)
    try {
      const response = await apiFetch(`/api/v1/nodes/${encodeURIComponent(targetNodeId)}/sessions/${encodeURIComponent(id)}/stop`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({}),
      })
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(responseError(data, `Could not end this session (${response.status}).`))
      setSession(sessionEnvelope(data))
      setSessionMessage('The session was ended on the target computer.')
    } catch (stopError) {
      setError(stopError instanceof Error ? stopError.message : 'Could not end the session.')
    } finally {
      setStoppingSession(false)
    }
  }

  const requiredReady = feasibility
    ? requiredStreams.every(stream => feasibility.feasible?.[stream.media_type] === true)
    : false
  const missingRequired = feasibility
    ? requiredStreams.filter(stream => feasibility.feasible?.[stream.media_type] !== true)
    : []
  const explainableAlternatives = new Set((routeExplanation?.streams ?? [])
    .filter(stream => Array.isArray(stream.pipelines) && stream.pipelines.length > 0)
    .map(stream => stream.media_type))
  const sessionState = session?.session?.state?.toLowerCase() ?? ''
  const canStartApprovedSession = sessionState === 'preparing'

  return (
    <div className="mx-auto max-w-4xl space-y-8 pb-12">
      <header className="space-y-2">
        <p className="text-sm font-medium uppercase tracking-[0.18em] text-oz-primary">Set up Ozma</p>
        <h1 className="text-3xl font-semibold text-oz-text">What would you like to do?</h1>
        <p className="max-w-2xl text-oz-muted">
          Choose an outcome first. Ozma will check the available connection paths; you do not need to choose a transport or codec.
        </p>
      </header>

      <section aria-label="Saved setups" className="rounded-2xl border border-oz-border bg-oz-surface p-5 sm:p-6">
        <div className="grid gap-4 sm:grid-cols-[1fr_1fr_auto] sm:items-end">
          <label className="space-y-2 text-sm text-oz-muted">
            <span className="block font-medium text-oz-text">Load a saved setup</span>
            <select value={savedSetupId} onChange={event => void loadSetup(event.target.value)} className="w-full rounded-lg border border-oz-border bg-oz-bg px-3 py-2.5 text-oz-text">
              <option value="">Start a new setup</option>
              {savedSetups.filter(item => item.setup_profile).map(item => <option key={item.id} value={item.id}>{item.name}</option>)}
            </select>
          </label>
          <label className="space-y-2 text-sm text-oz-muted">
            <span className="block font-medium text-oz-text">Setup name</span>
            <input value={setupName} onChange={event => setSetupName(event.target.value)} maxLength={120} className="w-full rounded-lg border border-oz-border bg-oz-bg px-3 py-2.5 text-oz-text" />
          </label>
          <button type="button" onClick={() => void saveSetup()} disabled={savingProfile || loading || !targetNodeId || !setupName.trim()} className="rounded-lg border border-oz-primary px-4 py-2.5 text-sm font-semibold text-oz-primary hover:bg-oz-primary/10 disabled:cursor-not-allowed disabled:opacity-40">
            {savingProfile ? 'Saving…' : savedSetupId ? 'Update saved setup' : 'Save setup'}
          </button>
        </div>
        <p className="mt-3 text-xs text-oz-muted">A saved setup remembers your goal and choices. It does not request access, grant permissions, or start a session.</p>
        {profileMessage && <p role="status" className="mt-2 text-sm text-emerald-200">{profileMessage}</p>}
      </section>

      {error && (
        <div role="alert" className="rounded-xl border border-red-500/40 bg-red-500/10 p-4 text-sm text-red-200">
          {error}
        </div>
      )}

      <section aria-labelledby="goal-heading" className="space-y-3">
        <h2 id="goal-heading" className="text-lg font-medium text-oz-text">Choose a goal</h2>
        <div className="grid gap-3 md:grid-cols-2">
          {GOALS.map(item => {
            const chosen = item.id === selectedGoal
            return (
              <button
                key={item.id}
                type="button"
                aria-pressed={chosen}
                onClick={() => chooseGoal(item.id)}
                className={`min-h-40 rounded-2xl border p-5 text-left transition-colors ${chosen ? 'border-oz-primary bg-oz-primary/10 ring-1 ring-oz-primary/50' : 'border-oz-border bg-oz-surface hover:border-oz-primary/60'}`}
              >
                <span className="mb-4 flex h-10 w-10 items-center justify-center rounded-xl bg-oz-bg text-xl text-oz-primary" aria-hidden="true">{item.icon}</span>
                <span className="block font-semibold text-oz-text">{item.title}</span>
                <span className="mt-2 block text-sm leading-relaxed text-oz-muted">{item.description}</span>
              </button>
            )
          })}
        </div>
        <div className="mt-3 grid gap-3 md:grid-cols-2">
          <button
            type="button"
            aria-expanded={showInboundInstructions}
            onClick={() => setShowInboundInstructions(value => !value)}
            className="rounded-2xl border border-oz-border bg-oz-surface p-5 text-left transition-colors hover:border-oz-primary/60 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-oz-primary"
          >
            <span className="mb-3 flex h-10 w-10 items-center justify-center rounded-xl bg-oz-bg text-xl text-oz-primary" aria-hidden="true">⇢</span>
            <span className="block font-semibold text-oz-text">Have another computer connect to this one</span>
            <span className="mt-2 block text-sm leading-relaxed text-oz-muted">Start from the other computer, then review its access request here.</span>
          </button>
          {showInboundInstructions && (
            <div role="region" aria-label="Allow another computer to connect" className="rounded-2xl border border-oz-primary/30 bg-oz-primary/5 p-5 text-sm leading-relaxed text-oz-muted">
              <p className="font-medium text-oz-text">The other computer starts the setup</p>
              <ol className="mt-2 list-decimal space-y-1 pl-5">
                <li>On that computer, open Ozma setup and choose <span className="text-oz-text">Control another computer</span>.</li>
                <li>Select this computer as the target and request access.</li>
                <li>Here, right-click the Ozma tray icon and choose <span className="text-oz-text">Review access on this computer</span>.</li>
              </ol>
              <p className="mt-2 text-xs">This setup page only sends requests as the signed-in controller. The target reviews and approves each request here.</p>
            </div>
          )}
        </div>
      </section>

      <section aria-labelledby="machines-heading" className="rounded-2xl border border-oz-border bg-oz-surface p-5 sm:p-6">
        <div className="mb-5">
          <h2 id="machines-heading" className="text-lg font-medium text-oz-text">Which computer do you want to use?</h2>
          <p className="mt-1 text-sm text-oz-muted">Choose the computer Ozma should connect to. Availability checks start from {routeCheckSourceLabel}.</p>
          {source !== 'controller' && !routeCheckSource && <p role="status" className="mt-2 text-sm text-amber-100">This saved starting computer is unavailable. Open Expert to choose another starting point before checking availability.</p>}
        </div>
        {loading ? (
          <p className="text-sm text-oz-muted">Looking for connected computers…</p>
        ) : endpointOptions.length === 0 ? (
          <div className="rounded-xl border border-amber-400/30 bg-amber-400/5 p-4 text-sm text-amber-100">
            No configured Ozma endpoints are available yet. Connect a computer to Ozma, then return here to check a setup.
          </div>
        ) : (
          <div className="grid gap-4 md:grid-cols-2">
            <label className="space-y-2 text-sm text-oz-muted md:max-w-xl">
              <span className="block font-medium text-oz-text">Computer</span>
              <select value={destination} onChange={event => { setDestination(event.target.value); setFeasibility(null); setRouteExplanation(null); setSession(null) }} className="w-full rounded-lg border border-oz-border bg-oz-bg px-3 py-2.5 text-oz-text focus:outline-none focus:ring-2 focus:ring-oz-primary">
                <option value="">Choose a computer</option>
                {endpointOptions.map(device => <option key={device.id} value={device.id}>{displayName(device)}</option>)}
              </select>
            </label>
          </div>
        )}
        <div className="mt-5 flex flex-wrap items-center gap-3">
          <button
            type="button"
            onClick={() => void checkSetup()}
            disabled={loading || checking || !source || (source !== 'controller' && !routeCheckSource) || !destination || endpointOptions.length < 2}
            className="rounded-lg bg-oz-primary px-4 py-2.5 text-sm font-semibold text-white transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40"
          >
            {checking ? 'Checking…' : 'Check if Ozma can do this'}
          </button>
          <span className="text-xs text-oz-muted">Ozma checks whether your goal is available for this computer. You can request access after reviewing the setup.</span>
        </div>
        {detailLevel === 'expert' && (
          <details className="mt-4 rounded-xl border border-oz-border bg-oz-bg p-4">
            <summary className="cursor-pointer text-sm font-medium text-oz-text">Advanced route check</summary>
            <p className="mt-2 text-xs leading-relaxed text-oz-muted">Choose a different starting endpoint for availability checks. Access requests still come from the signed-in controller.</p>
            <label className="mt-3 block max-w-xl space-y-2 text-sm text-oz-muted">
              <span className="block font-medium text-oz-text">Check availability from</span>
              <select value={source} onChange={event => { setSource(event.target.value); setFeasibility(null); setRouteExplanation(null); setSession(null) }} className="w-full rounded-lg border border-oz-border bg-oz-surface px-3 py-2.5 text-oz-text focus:outline-none focus:ring-2 focus:ring-oz-primary">
                {endpointOptions.map(device => <option key={device.id} value={device.id}>{displayName(device)}</option>)}
              </select>
            </label>
          </details>
        )}
      </section>

      <section aria-labelledby="detail-level-heading" className="rounded-2xl border border-oz-border bg-oz-surface p-5 sm:p-6">
        <div className="mb-5">
          <p className="text-xs font-medium uppercase tracking-wider text-oz-primary">Choose your level of detail</p>
          <h2 id="detail-level-heading" className="mt-1 text-lg font-medium text-oz-text">Start simple, open more controls when you need them</h2>
          <p className="mt-1 text-sm text-oz-muted">The same goal stays selected at every level. More detail reveals choices; it does not silently grant extra access.</p>
          <button type="button" onClick={resetToRecommended} className="mt-3 rounded-lg border border-oz-border px-3 py-2 text-sm font-medium text-oz-muted hover:border-oz-primary/50 hover:text-oz-text focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-oz-primary">
            Restore recommended choices
          </button>
        </div>
        <div role="group" aria-label="Configuration detail" className="mb-5 flex flex-wrap gap-2">
          {([
            ['simple', 'Simple', 'Choose an outcome'],
            ['guided', 'Guided', 'Choose what to share'],
            ['fine', 'Fine-tune', 'More supported controls'],
            ['expert', 'Expert', 'Limits and decision details'],
          ] as const).map(([level, label, description]) => (
            <button
              key={level}
              type="button"
              aria-pressed={detailLevel === level}
              onClick={() => setDetailLevel(level)}
              className={`rounded-xl border px-4 py-2.5 text-left ${detailLevel === level ? 'border-oz-primary bg-oz-primary/10 text-oz-text' : 'border-oz-border bg-oz-bg text-oz-muted hover:border-oz-primary/50'}`}
            >
              <span className="block text-sm font-semibold">{label}</span>
              <span className="mt-0.5 block text-xs">{description}</span>
            </button>
          ))}
        </div>
        <p className="-mt-3 mb-4 text-xs leading-relaxed text-oz-muted">Choose how much setup detail to see. Opening or closing this view does not change your goal or grant access. Restore recommended choices resets supported session settings without changing your goal or detail level. More image settings will appear here when this connection can apply them.</p>

        {detailLevel === 'simple' ? (
          <div className="rounded-xl border border-oz-border bg-oz-bg p-4 text-sm">
            <p className="font-medium text-oz-text">Recommended for “{goal.title}”</p>
            <p className="mt-1 text-oz-muted">{control ? 'Keyboard and mouse control' : 'No keyboard or mouse input'}{audio ? ' and audio' : ''}. View access is required.</p>
            <button type="button" onClick={() => setDetailLevel('guided')} className="mt-3 text-sm font-medium text-oz-primary hover:underline">Choose capabilities</button>
          </div>
        ) : (
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="flex items-center justify-between gap-4 rounded-xl border border-oz-border bg-oz-bg p-4">
              <div><p className="font-medium text-oz-text">View the computer</p><p className="mt-1 text-xs text-oz-muted">Required to see the remote screen.</p></div>
              <span className="rounded-full bg-emerald-400/10 px-2.5 py-1 text-xs font-semibold text-emerald-200">Required</span>
            </div>
            <CapabilityToggle title="Keyboard and mouse" detail="Send input to the remote computer." checked={control} onChange={value => { setControl(value); setFeasibility(null); setRouteExplanation(null); setSession(null) }} />
            <CapabilityToggle title="Listen to audio" detail="Receive sound from the remote computer." checked={audio} onChange={value => { setAudio(value); if (!value) setMicrophone(false); setSession(null) }} />
          </div>
        )}

        {(detailLevel === 'fine' || detailLevel === 'expert') && (
          <div className="mt-3 grid gap-3 sm:grid-cols-2">
            <CapabilityToggle title="Send microphone audio" detail="Share this device’s microphone with the session." checked={microphone} onChange={value => { setMicrophone(value); setSession(null) }} disabled={!audio} />
            <CapabilityToggle title="Clipboard" detail="Allow reading and writing clipboard content." checked={clipboard} onChange={value => { setClipboard(value); setSession(null) }} />
            <label className="rounded-xl border border-oz-border bg-oz-bg p-4 text-sm">
              <span className="block font-medium text-oz-text">Motion smoothness</span>
              <span className="mt-1 block text-xs text-oz-muted">Set a frame-rate ceiling for this session. A higher ceiling makes fast movement smoother; the actual rate can be lower.</span>
              <select value={maxFps} onChange={event => { setMaxFps(Number(event.target.value) as 15 | 24 | 30); setSession(null) }} className="mt-3 w-full rounded-lg border border-oz-border bg-oz-surface px-3 py-2 text-oz-text">
                <option value={15}>Lower motion rate · 15 frames per second</option>
                <option value={24}>Balanced · 24 frames per second</option>
                <option value={30}>Smoother motion · 30 frames per second</option>
              </select>
            </label>
            <label className="rounded-xl border border-oz-border bg-oz-bg p-4 text-sm">
              <span className="block font-medium text-oz-text">Image detail and data use</span>
              <span className="mt-1 block text-xs text-oz-muted">Set the most data this session can use for video. Higher limits can preserve more picture detail; actual use may be lower.</span>
              <select value={maxBitrateKbps} onChange={event => { setMaxBitrateKbps(Number(event.target.value) as 1000 | 2000 | 4000); setSession(null) }} className="mt-3 w-full rounded-lg border border-oz-border bg-oz-surface px-3 py-2 text-oz-text">
                <option value={1000}>Use less data · up to 1 Mbps</option>
                <option value={2000}>Balanced · up to 2 Mbps</option>
                <option value={4000}>More picture detail · up to 4 Mbps</option>
              </select>
            </label>
            <label className="rounded-xl border border-oz-border bg-oz-bg p-4 text-sm sm:col-span-2">
              <span className="block font-medium text-oz-text">Session access duration</span>
              <span className="mt-1 block text-xs text-oz-muted">How long this access request remains valid.</span>
              <select value={sessionLength} onChange={event => { setSessionLength(Number(event.target.value)); setSession(null) }} className="mt-3 w-full max-w-xs rounded-lg border border-oz-border bg-oz-surface px-3 py-2 text-oz-text">
                <option value={900}>15 minutes</option>
                <option value={3600}>1 hour</option>
                <option value={14400}>4 hours</option>
                <option value={28800}>8 hours</option>
                {![900, 3600, 14400, 28800].includes(sessionLength) && <option value={sessionLength}>{sessionLength} seconds (saved setup)</option>}
              </select>
            </label>
          </div>
        )}

        {(detailLevel === 'fine' || detailLevel === 'expert') && (
          <div className="mt-3 rounded-xl border border-amber-400/25 bg-amber-400/5 p-4 text-sm text-amber-100">
            <p className="font-medium">Some image-quality options are unavailable for this connection</p>
            <p className="mt-1 text-xs leading-relaxed">Motion rate and a video data ceiling are available above. Resolution, quality regions, and foveation cannot be set through this connection yet. Foveation is an advanced image-quality choice: it prioritizes detail near a gaze or focus region and spends less detail on the periphery. These options will become selectable when the connection can apply them and report the effective result.</p>
          </div>
        )}

        {detailLevel === 'expert' && (
          <div className="mt-4 space-y-3 rounded-xl border border-oz-border bg-oz-bg p-4 text-sm">
            <p className="font-medium text-oz-text">What this runtime can apply</p>
            <p className="text-oz-muted">This session path can request view, keyboard/mouse, audio, microphone, and clipboard access. The node reports what it actually grants and the effective result.</p>
            <p className="text-xs text-oz-muted">Selecting a more detailed view reveals controls and explanations; it does not make unsupported settings active.</p>
            <p className="text-xs text-oz-muted">File transfer, USB, power actions, and commands are also unavailable in this session setup.</p>
          </div>
        )}
      </section>

      <section aria-labelledby="preview-heading" className="rounded-2xl border border-oz-border bg-oz-surface p-5 sm:p-6">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <p className="text-xs font-medium uppercase tracking-wider text-oz-primary">Setup preview</p>
            <h2 id="preview-heading" className="mt-1 text-xl font-semibold text-oz-text">{goal.title}</h2>
            <p className="mt-2 max-w-2xl text-sm text-oz-muted">{goal.description}</p>
          </div>
          {feasibility && (
            <span className={`rounded-full px-3 py-1 text-xs font-semibold ${requiredReady ? 'bg-emerald-400/10 text-emerald-300' : 'bg-amber-400/10 text-amber-200'}`}>
              {requiredReady ? 'Required paths available' : 'Setup needs attention'}
            </span>
          )}
        </div>

        <div className="mt-5 space-y-4">
          <div>
            <h3 className="mb-2 text-sm font-medium text-oz-text">Needed for this goal</h3>
            {requiredStreams.length ? (
              <ul className="flex flex-wrap gap-2">
                {requiredStreams.map(stream => {
                  const isAvailable = feasibility?.feasible?.[stream.media_type]
                  return (
                    <li key={stream.media_type} className={`rounded-lg border px-3 py-1.5 text-sm ${isAvailable === true ? 'border-emerald-400/30 bg-emerald-400/5 text-emerald-200' : isAvailable === false ? 'border-amber-400/30 bg-amber-400/5 text-amber-100' : 'border-oz-border bg-oz-bg text-oz-muted'}`}>
                      {MEDIA_LABELS[stream.media_type] ?? stream.media_type}
                      {isAvailable === true ? ' · available' : isAvailable === false ? ' · not available yet' : ''}
                    </li>
                  )
                })}
              </ul>
            ) : <p className="text-sm text-oz-muted">Choose a goal to see what it needs.</p>}
          </div>
          {optionalStreams.length > 0 && (
            <div>
              <h3 className="mb-2 text-sm font-medium text-oz-text">Optional</h3>
              <ul className="flex flex-wrap gap-2">
                {optionalStreams.map(stream => (
                  <li key={stream.media_type} className="rounded-lg border border-oz-border bg-oz-bg px-3 py-1.5 text-sm text-oz-muted">
                    {MEDIA_LABELS[stream.media_type] ?? stream.media_type}
                  </li>
                ))}
              </ul>
            </div>
          )}
          {feasibility && !requiredReady && (
            <div className="space-y-3 rounded-lg bg-amber-400/5 p-3 text-sm leading-relaxed text-amber-100">
              <p>Ozma can’t complete this setup between those computers yet. Missing: {missingRequired.map(stream => MEDIA_LABELS[stream.media_type] ?? stream.media_type).join(', ')}.</p>
              {missingRequired.some(stream => ['video', 'screen'].includes(stream.media_type)) && (
                <p>Choose a computer that can share its screen, or enable screen sharing on the target.</p>
              )}
              {missingRequired.some(stream => stream.media_type === 'hid') && (
                <p>Keyboard and mouse control is not available on this route. You can continue in view-only mode if picture is available.</p>
              )}
              {selectedGoal === 'control' && explainableAlternatives.has('video') && (
                <button type="button" onClick={() => chooseGoal('observe')} className="rounded-lg border border-amber-200/40 px-3 py-2 font-medium hover:bg-amber-100/10">Continue view-only</button>
              )}
              <p className="text-xs text-amber-100/80">This checks connection availability only. The target still reviews access requests separately.</p>
            </div>
          )}
          {feasibility && requiredReady && (
            <p className="rounded-lg bg-emerald-400/5 p-3 text-sm leading-relaxed text-emerald-100">
              Ozma found the required route types. A route check is not authorization: the session still needs the appropriate access grant and any required consent.
            </p>
          )}
        </div>
      </section>

      <section aria-labelledby="review-heading" className="rounded-2xl border border-oz-border bg-oz-surface p-5 sm:p-6">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <p className="text-xs font-medium uppercase tracking-wider text-oz-primary">Permission review</p>
            <h2 id="review-heading" className="mt-1 text-lg font-medium text-oz-text">Check what will be allowed</h2>
            <p className="mt-1 text-sm text-oz-muted">The target computer may need someone there to approve access. The request asks for an activity indicator, but this session path does not confirm whether the target displays one. This request does not start the session or save a reusable profile.</p>
          </div>
          <button
            type="button"
            onClick={() => void requestAccess()}
            disabled={requesting || !targetNodeId}
            className="rounded-lg bg-oz-primary px-4 py-2.5 text-sm font-semibold text-zinc-950 transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40"
          >
            {requesting ? 'Checking with computer…' : session ? 'Request again' : 'Request this access'}
          </button>
        </div>

        {!targetNodeId && <p className="mt-3 text-xs text-amber-100">Choose a connected computer above to request access.</p>}
        <div className="mt-5 grid gap-5 md:grid-cols-2">
          <PermissionSummary title="Selected / requested" permissions={requestedPermissions} />
          <div>
            <h3 className="text-xs font-semibold uppercase tracking-wider text-oz-muted">Permissions recorded for this request</h3>
            {grantedPermissions ? <PermissionSummary permissions={grantedPermissions} /> : <p className="mt-2 text-sm text-oz-muted">No session request has been confirmed.</p>}
          </div>
        </div>
        <p className="mt-3 text-sm text-oz-muted">Motion preference: up to {maxFps} frames per second. Video data ceiling: up to {maxBitrateKbps / 1000} Mbps.</p>

        <div className={`mt-4 rounded-lg border p-3 text-sm ${targetReportsVideo ? 'border-oz-border bg-oz-bg text-oz-muted' : 'border-amber-400/25 bg-amber-400/5 text-amber-100'}`}>
          {targetReportsVideo
            ? 'This computer reports a screen source. Ozma will resolve its default source when the session starts; this unstarted request does not verify that a frame can be captured.'
            : 'This computer has not reported a screen source. View access is requested, but screen capture is unconfirmed and may be unavailable until the computer provides a source.'}
        </div>

        <div className="mt-5 border-t border-oz-border pt-4" aria-live="polite">
          <h3 className="text-xs font-semibold uppercase tracking-wider text-oz-muted">Node policy evaluation</h3>
          {!session ? (
            <p className="mt-2 text-sm text-oz-muted">Not evaluated yet. Request access to ask the node for the effective permissions.</p>
          ) : effectivePermissions ? (
            <>
              <PermissionSummary permissions={effectivePermissions} />
              {session.aperture?.limiting?.length ? <LimitingSummary receipt={session.aperture} /> : <p className="mt-3 text-sm text-emerald-200">The node reports these permissions as allowed by policy if the session is approved and started. No access is active yet.</p>}
            </>
          ) : (
            <p className="mt-2 text-sm text-amber-100">The node did not return an aperture receipt. Effective access is unconfirmed; this request must not be treated as granted.</p>
          )}
          {session?.session?.state && (
            <div className="mt-3 space-y-2">
              <p className="text-xs text-oz-muted">
                Request state: <span className="text-oz-text">{friendlyState(session.session.state)}</span>.{' '}
                {sessionState === 'pending_consent'
                  ? 'Approve this request on the target computer before starting it here.'
                  : sessionState === 'preparing'
                    ? 'The target approved this request. Start it here when you are ready.'
                    : ['ready', 'streaming', 'degraded'].includes(sessionState)
                      ? 'The session is ready; this status does not confirm that media or input is connected.'
                      : 'This request is not currently active.'}
              </p>
              <div className="flex flex-wrap gap-2">
                {sessionState === 'pending_consent' && <button type="button" onClick={() => void refreshSession()} disabled={refreshingSession || startingSession || stoppingSession} className="rounded-lg border border-oz-border px-3 py-2 text-xs font-medium text-oz-text hover:border-oz-primary disabled:opacity-50">{refreshingSession ? 'Refreshing…' : 'Refresh approval status'}</button>}
                {canStartApprovedSession && <button type="button" onClick={() => void startSession()} disabled={startingSession || refreshingSession || stoppingSession} className="rounded-lg bg-oz-primary px-3 py-2 text-xs font-semibold text-white disabled:opacity-50">{startingSession ? 'Starting…' : 'Start approved session'}</button>}
                {['ready', 'streaming', 'degraded'].includes(sessionState) && <>
                  <button type="button" onClick={() => void refreshSession()} disabled={refreshingSession || startingSession || stoppingSession} className="rounded-lg border border-oz-border px-3 py-2 text-xs font-medium text-oz-text hover:border-oz-primary disabled:opacity-50">{refreshingSession ? 'Refreshing…' : 'Refresh session status'}</button>
                  <button type="button" onClick={() => void stopSession()} disabled={stoppingSession || startingSession || refreshingSession} className="rounded-lg border border-red-400/40 px-3 py-2 text-xs font-medium text-red-200 hover:border-red-300 disabled:opacity-50">{stoppingSession ? 'Ending…' : 'End session'}</button>
                </>}
              </div>
              {sessionMessage && <p role="status" className="text-xs text-emerald-200">{sessionMessage}</p>}
            </div>
          )}
          {session && (
            <div className="mt-4 rounded-lg border border-oz-border bg-oz-bg p-3 text-sm">
              <h3 className="text-xs font-semibold uppercase tracking-wider text-oz-muted">Image quality limits</h3>
              {session.quality?.effective?.max_fps ? (
                <p className="mt-2 text-oz-text">
                  Frame-rate ceiling: {session.quality.effective.max_fps} frames per second
                  {session.quality.requested?.max_fps && session.quality.requested.max_fps !== session.quality.effective.max_fps
                    ? ` (requested ${session.quality.requested.max_fps}; limited by ${session.quality.limiting?.find(item => item.max_fps !== undefined)?.source ?? 'the target encoder'})`
                    : ' (accepted by the target encoder)'}.
                </p>
              ) : (
                <p className="mt-2 text-amber-100">The target did not report the effective frame-rate ceiling. Do not assume the requested value was applied.</p>
              )}
              {session.quality?.effective?.max_bitrate_kbps ? (
                <p className="mt-2 text-oz-text">
                  Video data ceiling: {session.quality.effective.max_bitrate_kbps / 1000} Mbps
                  {session.quality.requested?.max_bitrate_kbps && session.quality.requested.max_bitrate_kbps !== session.quality.effective.max_bitrate_kbps
                    ? ` (requested ${session.quality.requested.max_bitrate_kbps / 1000} Mbps; limited by ${session.quality.limiting?.find(item => item.max_bitrate_kbps !== undefined)?.source ?? 'the target node'})`
                    : ' (accepted by the target node)'}.
                </p>
              ) : (
                <p className="mt-2 text-amber-100">The target did not report the effective video data ceiling. Do not assume the requested value was applied.</p>
              )}
            </div>
          )}
        </div>

        {detailLevel === 'expert' && session?.session?.id && (
          <details className="mt-4 rounded-lg border border-oz-border p-3 text-xs text-oz-muted">
            <summary className="cursor-pointer font-medium text-oz-text">Expert session details</summary>
            <p className="mt-2 break-all">Session reference: {session.session.id}</p>
          </details>
        )}
      </section>

      {detailLevel === 'expert' && (
        <details className="rounded-xl border border-oz-border bg-oz-surface/60 p-4 text-sm text-oz-muted">
          <summary className="cursor-pointer font-medium text-oz-text">Expert configuration details</summary>
          <div className="mt-3 space-y-2">
            <p>Routing profile: <code className="text-oz-primary">{routingIntent}</code>{intent?.description ? ` — ${intent.description}` : ''}</p>
            {feasibility && <p>Connection path reference: <code>{feasibility.source}</code> → <code>{feasibility.destination}</code></p>}
            <p>Session access requests are evaluated by the node. Saved setup profiles preserve intent, motion and video-data preferences, and detail level, but do not grant access or start a session. The target reports effective frame-rate and video-data ceilings. Resolution, quality-region, and foveation settings are not sent by this session path.</p>
            {session?.aperture?.policy_version && <p>Permission receipt: <code>{session.aperture.policy_version}</code>{session.aperture.input_hash ? <> · reference <code>{session.aperture.input_hash}</code></> : null}</p>}
            {session?.quality?.policy_receipt?.policy_version && <p>Quality receipt: <code>{session.quality.policy_receipt.policy_version}</code>{session.quality.policy_receipt.input_hash ? <> · reference <code>{session.quality.policy_receipt.input_hash}</code></> : null}</p>}
          </div>
        </details>
      )}
    </div>
  )
}

function CapabilityToggle({
  title,
  detail,
  checked,
  onChange,
  disabled = false,
}: {
  title: string
  detail: string
  checked: boolean
  onChange: (checked: boolean) => void
  disabled?: boolean
}) {
  return (
    <label className={`flex items-center justify-between gap-4 rounded-xl border border-oz-border bg-oz-bg p-4 ${disabled ? 'cursor-not-allowed opacity-50' : 'cursor-pointer'}`}>
      <span>
        <span className="block font-medium text-oz-text">{title}</span>
        <span className="mt-1 block text-xs text-oz-muted">{detail}</span>
      </span>
      <input type="checkbox" checked={checked} disabled={disabled} onChange={event => onChange(event.target.checked)} className="h-4 w-4 accent-emerald-400" />
    </label>
  )
}

function PermissionSummary({ title, permissions }: { title?: string; permissions: string[] }) {
  return (
    <div>
      {title && <h3 className="text-xs font-semibold uppercase tracking-wider text-oz-muted">{title}</h3>}
      {permissions.length ? (
        <ul className="mt-2 flex flex-wrap gap-2">
          {permissions.map(permission => (
            <li key={permission} className="rounded-lg border border-emerald-400/25 bg-emerald-400/5 px-2.5 py-1.5 text-xs text-emerald-100">
              {PERMISSION_LABELS[permission as PermissionName] ?? 'Additional capability'}
            </li>
          ))}
        </ul>
      ) : <p className="mt-2 text-sm text-oz-muted">No capabilities are reported.</p>}
    </div>
  )
}

function LimitingSummary({ receipt }: { receipt: ApertureReceipt }) {
  const frames = [...new Set((receipt.limiting ?? []).map(item => item.frame).filter((frame): frame is string => Boolean(frame)))]
  const recourse = receipt.recourse
  return (
    <div className="mt-3 rounded-lg border border-amber-400/25 bg-amber-400/5 p-3 text-sm text-amber-100">
      <p>Some requested access is limited by the current node policy{frames.length ? ` (${frames.map(frame => frame === 'session-grant' ? 'session grant' : frame).join(', ')})` : ''}.</p>
      {recourse && <p className="mt-1">{recourse === 'escalate' ? 'An operator or device policy change is needed.' : recourse === 'propose' ? 'The other participant must approve a change.' : 'You can change your selection and request again.'}</p>}
    </div>
  )
}
