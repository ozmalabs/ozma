import { useState } from 'react'
import AppLayout from '../layouts/AppLayout'
import { useStreams, StreamInfo, StreamStats, RecordingStatus } from '../hooks/useStreams'

// ── helpers ──────────────────────────────────────────────────────────────────

function fmtUptime(s: number): string {
  if (s < 60) return `${Math.floor(s)}s`
  if (s < 3600) return `${Math.floor(s / 60)}m ${Math.floor(s % 60)}s`
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`
}

function fmtBytes(b: number): string {
  if (b < 1024) return `${b} B`
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`
  return `${(b / 1024 / 1024).toFixed(1)} MB`
}

// ── sub-components ────────────────────────────────────────────────────────────

interface StreamPreviewProps {
  stream: StreamInfo
  stats?: StreamStats
  recording?: RecordingStatus
  onSwitchCodec: (nodeId: string) => void
  onToggleRecord: (nodeId: string) => void
  onOpenOverlays: (nodeId: string) => void
}

function StreamPreview({
  stream,
  stats,
  recording,
  onSwitchCodec,
  onToggleRecord,
  onOpenOverlays,
}: StreamPreviewProps) {
  const isRecording = recording?.recording ?? false
  const mjpegSrc = `/api/v1/streams/${encodeURIComponent(stream.node_id)}/mjpeg`

  return (
    <div className="flex flex-col bg-zinc-900 border border-zinc-800 rounded-lg overflow-hidden">
      {/* Preview area */}
      <div className="relative bg-black aspect-video flex items-center justify-center">
        {stream.active ? (
          <img
            src={mjpegSrc}
            alt={`Stream: ${stream.node_id}`}
            className="w-full h-full object-contain"
            onError={(e) => {
              ;(e.target as HTMLImageElement).style.display = 'none'
            }}
          />
        ) : (
          <span className="text-zinc-600 text-sm font-mono">No signal</span>
        )}

        {/* Recording badge */}
        {isRecording && (
          <span className="absolute top-2 left-2 flex items-center gap-1.5 bg-red-600/90 text-white text-xs font-semibold px-2 py-0.5 rounded">
            <span className="w-2 h-2 rounded-full bg-white animate-pulse" />
            REC
          </span>
        )}

        {/* Stream type badge */}
        <span className="absolute top-2 right-2 bg-zinc-800/80 text-zinc-400 text-xs px-2 py-0.5 rounded font-mono">
          {stream.type}
        </span>
      </div>

      {/* Info bar */}
      <div className="px-3 py-2 border-b border-zinc-800">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium text-zinc-100 truncate">{stream.node_id}</span>
          <span
            className={`text-xs font-mono ${stream.active ? 'text-emerald-400' : 'text-zinc-500'}`}
          >
            {stream.active ? 'live' : 'offline'}
          </span>
        </div>
        {stats && (
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-zinc-500 font-mono">
            <span>{stats.encoder || '—'}</span>
            <span>{stats.fps_actual.toFixed(1)} fps</span>
            <span>{fmtUptime(stats.uptime_s)}</span>
            {stats.restarts > 0 && (
              <span className="text-amber-500">{stats.restarts} restart{stats.restarts !== 1 ? 's' : ''}</span>
            )}
          </div>
        )}
        {isRecording && recording && (
          <div className="mt-1 text-xs text-red-400 font-mono">
            {recording.filename ?? 'recording'}{' '}
            {recording.duration_s !== undefined && `· ${fmtUptime(recording.duration_s)}`}{' '}
            {recording.size_bytes !== undefined && `· ${fmtBytes(recording.size_bytes)}`}
          </div>
        )}
      </div>

      {/* Action buttons */}
      <div className="flex gap-2 px-3 py-2">
        <button
          onClick={() => onToggleRecord(stream.node_id)}
          className={`flex-1 text-xs py-1.5 rounded font-medium transition-colors ${
            isRecording
              ? 'bg-red-600 hover:bg-red-700 text-white'
              : 'bg-zinc-800 hover:bg-zinc-700 text-zinc-300'
          }`}
        >
          {isRecording ? '⏹ Stop' : '⏺ Record'}
        </button>
        <button
          onClick={() => onSwitchCodec(stream.node_id)}
          className="flex-1 text-xs py-1.5 rounded font-medium bg-zinc-800 hover:bg-zinc-700 text-zinc-300 transition-colors"
        >
          Codec
        </button>
        <button
          onClick={() => onOpenOverlays(stream.node_id)}
          className="flex-1 text-xs py-1.5 rounded font-medium bg-zinc-800 hover:bg-zinc-700 text-zinc-300 transition-colors"
        >
          Overlays
        </button>
      </div>
    </div>
  )
}

// ── Codec modal ───────────────────────────────────────────────────────────────

const CODEC_OPTIONS = [
  { codec: 'h264', hw: 'software', label: 'H.264 Software' },
  { codec: 'h264', hw: 'nvenc',    label: 'H.264 NVENC (NVIDIA)' },
  { codec: 'h264', hw: 'vaapi',    label: 'H.264 VAAPI (Intel/AMD)' },
  { codec: 'h265', hw: 'software', label: 'H.265 Software' },
  { codec: 'h265', hw: 'nvenc',    label: 'H.265 NVENC (NVIDIA)' },
  { codec: 'h265', hw: 'vaapi',    label: 'H.265 VAAPI (Intel/AMD)' },
  { codec: 'av1',  hw: 'software', label: 'AV1 Software' },
  { codec: 'ocr',  hw: 'software', label: 'OCR Terminal (zero bandwidth)' },
]

interface CodecModalProps {
  nodeId: string
  currentStats?: StreamStats
  onConfirm: (codec: string, hw: string) => Promise<void>
  onClose: () => void
}

function CodecModal({ nodeId, currentStats, onConfirm, onClose }: CodecModalProps) {
  const [selected, setSelected] = useState(0)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  async function handleConfirm() {
    setBusy(true)
    setErr(null)
    try {
      const opt = CODEC_OPTIONS[selected]
      await onConfirm(opt.codec, opt.hw)
      onClose()
    } catch (e) {
      setErr(e instanceof Error ? e.message : 'Failed')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-zinc-900 border border-zinc-700 rounded-xl w-full max-w-sm mx-4 p-5 shadow-2xl">
        <h2 className="text-base font-semibold text-zinc-100 mb-1">Switch Codec</h2>
        <p className="text-xs text-zinc-500 mb-4 font-mono">{nodeId}</p>
        {currentStats && (
          <div className="mb-4 text-xs text-zinc-400 font-mono bg-zinc-800 rounded px-3 py-2">
            Current: <span className="text-zinc-200">{currentStats.encoder}</span>
            {' · '}{currentStats.hw_type}{' · '}{currentStats.fps_actual.toFixed(1)} fps
          </div>
        )}
        <div className="flex flex-col gap-1.5 mb-4">
          {CODEC_OPTIONS.map((opt, i) => (
            <label
              key={i}
              className={`flex items-center gap-3 px-3 py-2 rounded cursor-pointer text-sm transition-colors ${
                selected === i
                  ? 'bg-emerald-400/10 text-emerald-300 border border-emerald-400/30'
                  : 'text-zinc-400 hover:bg-zinc-800 border border-transparent'
              }`}
            >
              <input
                type="radio"
                name="codec"
                checked={selected === i}
                onChange={() => setSelected(i)}
                className="accent-emerald-400"
              />
              {opt.label}
            </label>
          ))}
        </div>
        {err && <p className="text-xs text-red-400 mb-3">{err}</p>}
        <div className="flex gap-2 justify-end">
          <button
            onClick={onClose}
            className="px-4 py-1.5 text-sm rounded bg-zinc-800 hover:bg-zinc-700 text-zinc-300"
          >
            Cancel
          </button>
          <button
            onClick={handleConfirm}
            disabled={busy}
            className="px-4 py-1.5 text-sm rounded bg-emerald-600 hover:bg-emerald-500 text-white disabled:opacity-50"
          >
            {busy ? 'Switching…' : 'Apply'}
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Overlays modal ────────────────────────────────────────────────────────────

interface OverlaysModalProps {
  nodeId: string
  onClose: () => void
}

const DEFAULT_OVERLAYS = [
  { id: 'ts',   type: 'timestamp' as const, label: 'Timestamp', x: 10, y: 10, enabled: true },
  { id: 'lbl',  type: 'text'      as const, label: 'Node label', x: 10, y: 40, enabled: false },
]

function OverlaysModal({ nodeId, onClose }: OverlaysModalProps) {
  const [overlays, setOverlays] = useState(DEFAULT_OVERLAYS)
  const [busy, setBusy] = useState(false)
  const [saved, setSaved] = useState(false)

  function toggle(id: string) {
    setOverlays(prev => prev.map(o => o.id === id ? { ...o, enabled: !o.enabled } : o))
  }

  async function handleSave() {
    setBusy(true)
    try {
      await fetch(`/api/v1/streams/${encodeURIComponent(nodeId)}/overlays`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ overlays }),
      })
      setSaved(true)
      setTimeout(() => setSaved(false), 2000)
    } catch {
      // ignore
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-zinc-900 border border-zinc-700 rounded-xl w-full max-w-sm mx-4 p-5 shadow-2xl">
        <h2 className="text-base font-semibold text-zinc-100 mb-1">Overlays</h2>
        <p className="text-xs text-zinc-500 mb-4 font-mono">{nodeId}</p>
        <div className="flex flex-col gap-2 mb-4">
          {overlays.map(o => (
            <label
              key={o.id}
              className="flex items-center justify-between px-3 py-2 rounded bg-zinc-800 cursor-pointer"
            >
              <div>
                <span className="text-sm text-zinc-200">{o.label}</span>
                <span className="ml-2 text-xs text-zinc-500 font-mono">{o.type}</span>
              </div>
              <input
                type="checkbox"
                checked={o.enabled}
                onChange={() => toggle(o.id)}
                className="accent-emerald-400 w-4 h-4"
              />
            </label>
          ))}
        </div>
        <p className="text-xs text-zinc-600 mb-4">
          Overlay positions can be fine-tuned via the API (<code className="font-mono">/api/v1/streams/…/overlays</code>).
        </p>
        <div className="flex gap-2 justify-end">
          <button
            onClick={onClose}
            className="px-4 py-1.5 text-sm rounded bg-zinc-800 hover:bg-zinc-700 text-zinc-300"
          >
            Close
          </button>
          <button
            onClick={handleSave}
            disabled={busy}
            className="px-4 py-1.5 text-sm rounded bg-emerald-600 hover:bg-emerald-500 text-white disabled:opacity-50"
          >
            {saved ? '✓ Saved' : busy ? 'Saving…' : 'Save'}
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Main page ─────────────────────────────────────────────────────────────────

export default function StreamsPage() {
  const { streams, stats, recordings, loading, error, refresh, switchCodec, startRecording, stopRecording } =
    useStreams()

  const [codecModal, setCodecModal] = useState<string | null>(null)
  const [overlaysModal, setOverlaysModal] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)

  async function handleToggleRecord(nodeId: string) {
    setActionError(null)
    try {
      if (recordings[nodeId]?.recording) {
        await stopRecording(nodeId)
      } else {
        await startRecording(nodeId)
      }
    } catch (e) {
      setActionError(e instanceof Error ? e.message : 'Recording action failed')
    }
  }

  async function handleSwitchCodec(codec: string, hw: string) {
    if (!codecModal) return
    await switchCodec(codecModal, codec, hw)
  }

  const activeCount = streams.filter(s => s.active).length
  const recordingCount = Object.values(recordings).filter(r => r.recording).length

  return (
    <AppLayout>
      <div className="flex flex-col h-full">
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 border-b border-zinc-800 shrink-0">
          <div>
            <h1 className="text-lg font-semibold text-zinc-100">Streams &amp; Cameras</h1>
            <p className="text-xs text-zinc-500 mt-0.5">
              {activeCount} live · {streams.length} total
              {recordingCount > 0 && (
                <span className="ml-2 text-red-400 font-medium">
                  ⏺ {recordingCount} recording{recordingCount !== 1 ? 's' : ''}
                </span>
              )}
            </p>
          </div>
          <button
            onClick={refresh}
            className="text-xs px-3 py-1.5 rounded bg-zinc-800 hover:bg-zinc-700 text-zinc-300 transition-colors"
          >
            Refresh
          </button>
        </div>

        {/* Body */}
        <div className="flex-1 overflow-y-auto px-6 py-5">
          {actionError && (
            <div className="mb-4 px-4 py-2 rounded bg-red-900/40 border border-red-700 text-red-300 text-sm">
              {actionError}
            </div>
          )}

          {loading && (
            <div className="flex items-center justify-center h-40 text-zinc-500 text-sm">
              Loading streams…
            </div>
          )}

          {!loading && error && (
            <div className="flex flex-col items-center justify-center h-40 gap-2">
              <p className="text-red-400 text-sm">{error}</p>
              <button
                onClick={refresh}
                className="text-xs px-3 py-1.5 rounded bg-zinc-800 hover:bg-zinc-700 text-zinc-300"
              >
                Retry
              </button>
            </div>
          )}

          {!loading && !error && streams.length === 0 && (
            <div className="flex flex-col items-center justify-center h-40 text-zinc-500 text-sm gap-1">
              <span>No streams available.</span>
              <span className="text-xs text-zinc-600">
                Nodes with VNC or a stream URL will appear here automatically.
              </span>
            </div>
          )}

          {!loading && !error && streams.length > 0 && (
            <div className="grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-4 gap-4">
              {streams.map(stream => (
                <StreamPreview
                  key={stream.node_id}
                  stream={stream}
                  stats={stats[stream.node_id]}
                  recording={recordings[stream.node_id]}
                  onSwitchCodec={setCodecModal}
                  onToggleRecord={handleToggleRecord}
                  onOpenOverlays={setOverlaysModal}
                />
              ))}
            </div>
          )}
        </div>
      </div>

      {/* Modals */}
      {codecModal && (
        <CodecModal
          nodeId={codecModal}
          currentStats={stats[codecModal]}
          onConfirm={handleSwitchCodec}
          onClose={() => setCodecModal(null)}
        />
      )}
      {overlaysModal && (
        <OverlaysModal
          nodeId={overlaysModal}
          onClose={() => setOverlaysModal(null)}
        />
      )}
    </AppLayout>
  )
}
