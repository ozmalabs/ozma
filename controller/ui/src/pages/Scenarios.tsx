import { useState } from 'react'
import { useScenarios } from '../hooks/useScenarios'
import type { Scenario, ScenarioCreateRequest, ScenarioUpdateRequest } from '../types/node'

// ── Colour palette ────────────────────────────────────────────────────────────
const COLORS = [
  '#6366f1', '#8b5cf6', '#ec4899', '#ef4444',
  '#f97316', '#eab308', '#22c55e', '#14b8a6',
  '#3b82f6', '#06b6d4',
]

// ── Small helpers ─────────────────────────────────────────────────────────────
function ColorDot({ color, size = 'md' }: { color: string; size?: 'sm' | 'md' }) {
  const cls = size === 'sm' ? 'w-3 h-3' : 'w-4 h-4'
  return (
    <span
      className={`${cls} rounded-full inline-block flex-shrink-0`}
      style={{ backgroundColor: color }}
    />
  )
}

// ── Modal ─────────────────────────────────────────────────────────────────────
interface ModalProps {
  scenario?: Scenario | null
  onClose: () => void
  onSave: (data: ScenarioCreateRequest | ScenarioUpdateRequest) => Promise<void>
}

function ScenarioModal({ scenario, onClose, onSave }: ModalProps) {
  const [name, setName] = useState(scenario?.name ?? '')
  const [description, setDescription] = useState(scenario?.description ?? '')
  const [color, setColor] = useState(scenario?.color ?? COLORS[0])
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault()
    if (!name.trim()) { setError('Name is required'); return }
    setSaving(true)
    setError(null)
    try {
      await onSave({ name: name.trim(), description: description.trim() || undefined, color })
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Save failed')
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-oz-surface border border-oz-border rounded-xl shadow-2xl w-full max-w-md mx-4">
        <div className="flex items-center justify-between px-6 py-4 border-b border-oz-border">
          <h2 className="text-oz-text font-semibold text-lg">
            {scenario ? 'Edit Scenario' : 'New Scenario'}
          </h2>
          <button
            onClick={onClose}
            className="text-oz-muted hover:text-oz-text transition-colors text-xl leading-none"
          >
            ×
          </button>
        </div>

        <form onSubmit={handleSubmit} className="px-6 py-5 space-y-4">
          {/* Name */}
          <div>
            <label className="block text-oz-muted text-sm mb-1">Name</label>
            <input
              autoFocus
              value={name}
              onChange={e => setName(e.target.value)}
              placeholder="e.g. Gaming, Work, Presentation"
              className="w-full bg-oz-bg border border-oz-border rounded-lg px-3 py-2 text-oz-text placeholder-oz-muted focus:outline-none focus:ring-2 focus:ring-oz-accent"
            />
          </div>

          {/* Description */}
          <div>
            <label className="block text-oz-muted text-sm mb-1">Description <span className="text-oz-muted/60">(optional)</span></label>
            <textarea
              value={description}
              onChange={e => setDescription(e.target.value)}
              rows={2}
              placeholder="Short description…"
              className="w-full bg-oz-bg border border-oz-border rounded-lg px-3 py-2 text-oz-text placeholder-oz-muted focus:outline-none focus:ring-2 focus:ring-oz-accent resize-none"
            />
          </div>

          {/* Color */}
          <div>
            <label className="block text-oz-muted text-sm mb-2">Color</label>
            <div className="flex flex-wrap gap-2">
              {COLORS.map(c => (
                <button
                  key={c}
                  type="button"
                  onClick={() => setColor(c)}
                  className={`w-7 h-7 rounded-full transition-transform ${color === c ? 'ring-2 ring-offset-2 ring-oz-accent ring-offset-oz-surface scale-110' : 'hover:scale-110'}`}
                  style={{ backgroundColor: c }}
                />
              ))}
            </div>
          </div>

          {error && <p className="text-red-400 text-sm">{error}</p>}

          <div className="flex justify-end gap-3 pt-1">
            <button
              type="button"
              onClick={onClose}
              className="px-4 py-2 rounded-lg text-oz-muted hover:text-oz-text border border-oz-border hover:border-oz-text transition-colors text-sm"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={saving}
              className="px-4 py-2 rounded-lg bg-oz-accent text-white hover:bg-oz-accent/80 disabled:opacity-50 transition-colors text-sm font-medium"
            >
              {saving ? 'Saving…' : scenario ? 'Save Changes' : 'Create'}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}

// ── Delete confirmation ───────────────────────────────────────────────────────
interface DeleteConfirmProps {
  scenario: Scenario
  onClose: () => void
  onConfirm: () => Promise<void>
}

function DeleteConfirm({ scenario, onClose, onConfirm }: DeleteConfirmProps) {
  const [deleting, setDeleting] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function handleDelete() {
    setDeleting(true)
    setError(null)
    try {
      await onConfirm()
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Delete failed')
      setDeleting(false)
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-oz-surface border border-oz-border rounded-xl shadow-2xl w-full max-w-sm mx-4 px-6 py-5 space-y-4">
        <h2 className="text-oz-text font-semibold text-lg">Delete Scenario</h2>
        <p className="text-oz-muted text-sm">
          Are you sure you want to delete <span className="text-oz-text font-medium">"{scenario.name}"</span>? This cannot be undone.
        </p>
        {error && <p className="text-red-400 text-sm">{error}</p>}
        <div className="flex justify-end gap-3">
          <button
            onClick={onClose}
            className="px-4 py-2 rounded-lg text-oz-muted hover:text-oz-text border border-oz-border hover:border-oz-text transition-colors text-sm"
          >
            Cancel
          </button>
          <button
            onClick={handleDelete}
            disabled={deleting}
            className="px-4 py-2 rounded-lg bg-red-600 text-white hover:bg-red-500 disabled:opacity-50 transition-colors text-sm font-medium"
          >
            {deleting ? 'Deleting…' : 'Delete'}
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Scenario card ─────────────────────────────────────────────────────────────
interface CardProps {
  scenario: Scenario
  onEdit: (s: Scenario) => void
  onDelete: (s: Scenario) => void
  onActivate: (s: Scenario) => void
  activating: boolean
}

function ScenarioCard({ scenario, onEdit, onDelete, onActivate, activating }: CardProps) {
  return (
    <div
      className={`relative bg-oz-surface border rounded-xl p-5 flex flex-col gap-3 transition-all ${
        scenario.active
          ? 'border-oz-accent shadow-[0_0_0_1px] shadow-oz-accent'
          : 'border-oz-border hover:border-oz-border/80'
      }`}
    >
      {/* Active badge */}
      {scenario.active && (
        <span className="absolute top-3 right-3 text-xs font-semibold px-2 py-0.5 rounded-full bg-oz-accent/20 text-oz-accent">
          Active
        </span>
      )}

      {/* Header */}
      <div className="flex items-center gap-3 pr-16">
        <ColorDot color={scenario.color} />
        <span className="text-oz-text font-semibold truncate">{scenario.name}</span>
      </div>

      {/* Description */}
      {scenario.description && (
        <p className="text-oz-muted text-sm leading-snug line-clamp-2">{scenario.description}</p>
      )}

      {/* Actions */}
      <div className="flex items-center gap-2 mt-auto pt-1">
        {!scenario.active && (
          <button
            onClick={() => onActivate(scenario)}
            disabled={activating}
            className="flex-1 py-1.5 rounded-lg bg-oz-accent/10 text-oz-accent hover:bg-oz-accent/20 disabled:opacity-50 transition-colors text-sm font-medium"
          >
            {activating ? 'Activating…' : 'Activate'}
          </button>
        )}
        <button
          onClick={() => onEdit(scenario)}
          className="px-3 py-1.5 rounded-lg border border-oz-border text-oz-muted hover:text-oz-text hover:border-oz-text transition-colors text-sm"
        >
          Edit
        </button>
        <button
          onClick={() => onDelete(scenario)}
          className="px-3 py-1.5 rounded-lg border border-oz-border text-oz-muted hover:text-red-400 hover:border-red-400 transition-colors text-sm"
        >
          Delete
        </button>
      </div>
    </div>
  )
}

// ── Page ──────────────────────────────────────────────────────────────────────
export default function Scenarios() {
  const { scenarios, loading, error, refresh, createScenario, updateScenario, deleteScenario, activateScenario } =
    useScenarios()

  const [showCreate, setShowCreate] = useState(false)
  const [editTarget, setEditTarget] = useState<Scenario | null>(null)
  const [deleteTarget, setDeleteTarget] = useState<Scenario | null>(null)
  const [activatingId, setActivatingId] = useState<string | null>(null)

  async function handleActivate(scenario: Scenario) {
    setActivatingId(scenario.id)
    try {
      await activateScenario(scenario.id)
    } finally {
      setActivatingId(null)
    }
  }

  return (
    <div className="p-6 space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-oz-text text-2xl font-bold">Scenarios</h1>
          <p className="text-oz-muted text-sm mt-0.5">Manage and activate display / routing scenarios</p>
        </div>
        <button
          onClick={() => setShowCreate(true)}
          className="flex items-center gap-2 px-4 py-2 rounded-lg bg-oz-accent text-white hover:bg-oz-accent/80 transition-colors text-sm font-medium"
        >
          <span className="text-lg leading-none">+</span> New Scenario
        </button>
      </div>

      {/* Error banner */}
      {error && (
        <div className="flex items-center justify-between bg-red-900/30 border border-red-700 rounded-lg px-4 py-3">
          <span className="text-red-300 text-sm">{error}</span>
          <button onClick={refresh} className="text-red-300 hover:text-white text-sm underline ml-4">
            Retry
          </button>
        </div>
      )}

      {/* Loading */}
      {loading && (
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {[...Array(3)].map((_, i) => (
            <div key={i} className="bg-oz-surface border border-oz-border rounded-xl p-5 h-36 animate-pulse" />
          ))}
        </div>
      )}

      {/* Empty state */}
      {!loading && !error && scenarios.length === 0 && (
        <div className="flex flex-col items-center justify-center py-20 text-center">
          <div className="text-5xl mb-4">🎬</div>
          <p className="text-oz-text font-medium text-lg">No scenarios yet</p>
          <p className="text-oz-muted text-sm mt-1 mb-6">Create your first scenario to get started.</p>
          <button
            onClick={() => setShowCreate(true)}
            className="px-5 py-2 rounded-lg bg-oz-accent text-white hover:bg-oz-accent/80 transition-colors text-sm font-medium"
          >
            + New Scenario
          </button>
        </div>
      )}

      {/* Grid */}
      {!loading && scenarios.length > 0 && (
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {scenarios.map(s => (
            <ScenarioCard
              key={s.id}
              scenario={s}
              onEdit={setEditTarget}
              onDelete={setDeleteTarget}
              onActivate={handleActivate}
              activating={activatingId === s.id}
            />
          ))}
        </div>
      )}

      {/* Create modal */}
      {showCreate && (
        <ScenarioModal
          onClose={() => setShowCreate(false)}
          onSave={data => createScenario(data as ScenarioCreateRequest)}
        />
      )}

      {/* Edit modal */}
      {editTarget && (
        <ScenarioModal
          scenario={editTarget}
          onClose={() => setEditTarget(null)}
          onSave={data => updateScenario(editTarget.id, data as ScenarioUpdateRequest)}
        />
      )}

      {/* Delete confirmation */}
      {deleteTarget && (
        <DeleteConfirm
          scenario={deleteTarget}
          onClose={() => setDeleteTarget(null)}
          onConfirm={() => deleteScenario(deleteTarget.id)}
        />
      )}
    </div>
  )
}
