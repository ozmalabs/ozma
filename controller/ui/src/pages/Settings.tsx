import { useState, useEffect, FormEvent } from 'react'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface ConnectStatus {
  authenticated: boolean
  tier: string
  email?: string
  account_email?: string
  subdomain?: string
  usage?: { nodes: number; controllers: number }
  limits?: { controllers: number; nodes: number }
}

interface CurrentUser {
  id: string
  username: string
  display_name?: string
  role: string
  auth_enabled: boolean
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="bg-white rounded-xl shadow-sm border border-gray-200 p-6 space-y-4">
      <h2 className="text-base font-semibold text-gray-500 uppercase tracking-wide">{title}</h2>
      {children}
    </section>
  )
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="flex flex-col gap-1 text-sm text-gray-700">
      <span className="font-medium">{label}</span>
      {children}
    </label>
  )
}

const inputCls =
  'border border-gray-300 rounded-lg px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-indigo-500 bg-white'

const btnPrimary =
  'bg-indigo-600 text-white text-sm font-medium px-4 py-2 rounded-lg hover:bg-indigo-700 disabled:opacity-50 disabled:cursor-not-allowed'

const btnDanger =
  'border border-red-300 text-red-600 text-sm font-medium px-4 py-2 rounded-lg hover:bg-red-50 disabled:opacity-50'

const btnGhost =
  'border border-gray-300 text-gray-600 text-sm font-medium px-4 py-2 rounded-lg hover:bg-gray-50'

// ---------------------------------------------------------------------------
// Connect section
// ---------------------------------------------------------------------------

function ConnectSection() {
  const [status, setStatus] = useState<ConnectStatus | null>(null)
  const [loading, setLoading] = useState(true)
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const [working, setWorking] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [success, setSuccess] = useState<string | null>(null)
  const [subdomainInput, setSubdomainInput] = useState('')
  const [subdomainWorking, setSubdomainWorking] = useState(false)
  const [subdomainError, setSubdomainError] = useState<string | null>(null)

  useEffect(() => { fetchStatus() }, [])

  async function fetchStatus() {
    setLoading(true)
    try {
      const r = await fetch('/api/v1/connect/status')
      if (r.ok) setStatus(await r.json())
    } catch { /* ignore */ } finally { setLoading(false) }
  }

  async function handleLogin(e: FormEvent) {
    e.preventDefault()
    setError(null); setSuccess(null); setWorking(true)
    try {
      const r = await fetch('/api/v1/connect/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email, password }),
      })
      const data = await r.json()
      if (!r.ok) throw new Error(data.detail ?? data.error ?? 'Login failed')
      setSuccess(`Connected! Tier: ${data.tier}`)
      setPassword('')
      await fetchStatus()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed')
    } finally { setWorking(false) }
  }

  async function handleLogout() {
    setError(null); setSuccess(null)
    try {
      await fetch('/api/v1/connect/logout', { method: 'POST' })
      setSuccess('Disconnected from Ozma Connect.')
      await fetchStatus()
    } catch { setError('Logout failed') }
  }

  async function handleClaimSubdomain(e: FormEvent) {
    e.preventDefault()
    setSubdomainError(null)
    if (!subdomainInput.trim()) { setSubdomainError('Subdomain cannot be empty'); return }
    if (!/^[a-z0-9-]+$/.test(subdomainInput)) {
      setSubdomainError('Only lowercase letters, numbers, and hyphens allowed')
      return
    }
    setSubdomainWorking(true)
    try {
      const r = await fetch('/api/v1/subdomain/claim', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username: subdomainInput }),
      })
      const data = await r.json()
      if (!r.ok) throw new Error(data.detail ?? 'Claim failed')
      setSuccess(`Subdomain claimed: ${data.domain}`)
      await fetchStatus()
    } catch (err) {
      setSubdomainError(err instanceof Error ? err.message : 'Claim failed')
    } finally { setSubdomainWorking(false) }
  }

  if (loading) return <p className="text-sm text-gray-400">Loading Connect status…</p>

  const accountEmail = status?.account_email ?? status?.email

  return (
    <Section title="Ozma Connect">
      {error && <p className="text-sm text-red-600">{error}</p>}
      {success && <p className="text-sm text-green-600">{success}</p>}

      {status?.authenticated ? (
        <div className="space-y-3">
          <div className="flex items-center gap-2">
            <span className="w-2 h-2 rounded-full bg-green-500 flex-shrink-0" />
            <span className="text-sm font-medium text-gray-800">
              {accountEmail ?? 'Connected'}
            </span>
            <span className={`text-xs px-2 py-0.5 rounded-full font-medium ${
              status.tier === 'pro' ? 'bg-purple-100 text-purple-700'
              : status.tier === 'team' ? 'bg-blue-100 text-blue-700'
              : 'bg-gray-100 text-gray-600'
            }`}>
              {status.tier}
            </span>
          </div>

          {status.subdomain ? (
            <p className="text-sm text-gray-600">
              Subdomain:{' '}
              <a
                href={`https://${status.subdomain}`}
                target="_blank"
                rel="noopener noreferrer"
                className="text-indigo-600 hover:underline font-medium"
              >
                {status.subdomain}
              </a>
            </p>
          ) : (
            <form onSubmit={handleClaimSubdomain} className="space-y-2">
              <p className="text-sm text-gray-500">Claim a public subdomain for remote access:</p>
              <div className="flex gap-2 items-center">
                <input
                  type="text"
                  placeholder="your-name"
                  value={subdomainInput}
                  onChange={e => setSubdomainInput(e.target.value.toLowerCase())}
                  className={`${inputCls} flex-1`}
                />
                <span className="text-sm text-gray-400 whitespace-nowrap">.c.ozma.dev</span>
                <button type="submit" disabled={subdomainWorking} className={btnPrimary}>
                  {subdomainWorking ? 'Claiming…' : 'Claim'}
                </button>
              </div>
              {subdomainError && <p className="text-sm text-red-600">{subdomainError}</p>}
            </form>
          )}

          {status.usage && (
            <p className="text-xs text-gray-400">
              Nodes: {status.usage.nodes}
              {status.limits && status.limits.nodes > 0 ? ` / ${status.limits.nodes}` : ''}
            </p>
          )}

          <button onClick={handleLogout} className={btnDanger}>
            Disconnect account
          </button>
        </div>
      ) : (
        <form onSubmit={handleLogin} className="space-y-3">
          <p className="text-sm text-gray-600">
            Link this controller to Ozma Connect for remote access, relay, and cloud backup.
          </p>
          <Field label="Email">
            <input
              type="email"
              required
              value={email}
              onChange={e => setEmail(e.target.value)}
              className={inputCls}
              placeholder="you@example.com"
            />
          </Field>
          <Field label="Password">
            <input
              type="password"
              required
              value={password}
              onChange={e => setPassword(e.target.value)}
              className={inputCls}
              placeholder="••••••••"
            />
          </Field>
          <button type="submit" disabled={working} className={`${btnPrimary} w-full`}>
            {working ? 'Linking…' : 'Link to Ozma Connect'}
          </button>
          <p className="text-xs text-gray-400 text-center">
            No account?{' '}
            <a
              href="https://ozma.dev/signup"
              target="_blank"
              rel="noopener noreferrer"
              className="text-indigo-600 hover:underline"
            >
              Sign up free
            </a>
          </p>
        </form>
      )}
    </Section>
  )
}

// ---------------------------------------------------------------------------
// Change-password section
// ---------------------------------------------------------------------------

function ChangePasswordSection({ user }: { user: CurrentUser }) {
  const [newPass, setNewPass] = useState('')
  const [confirm, setConfirm] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [success, setSuccess] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  async function handleSubmit(e: FormEvent) {
    e.preventDefault()
    setError(null); setSuccess(null)
    if (!newPass) { setError('Password cannot be empty'); return }
    if (newPass.length < 8) { setError('Password must be at least 8 characters'); return }
    if (newPass !== confirm) { setError('Passwords do not match'); return }
    setLoading(true)
    try {
      const r = await fetch('/api/v1/users/me', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password: newPass }),
      })
      if (!r.ok) {
        const data = await r.json().catch(() => ({}))
        throw new Error(data.detail ?? data.error ?? `HTTP ${r.status}`)
      }
      setSuccess('Password updated.')
      setNewPass(''); setConfirm('')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update password')
    } finally { setLoading(false) }
  }

  return (
    <Section title="Account">
      <div className="text-sm text-gray-600 space-y-1">
        <p>Username: <span className="font-medium">{user.username}</span></p>
        <p>Role: <span className="font-medium capitalize">{user.role}</span></p>
      </div>

      <form onSubmit={handleSubmit} className="space-y-3 pt-3 border-t border-gray-100">
        <h3 className="text-sm font-semibold text-gray-700">Change Password</h3>
        <Field label="New password">
          <input
            type="password"
            value={newPass}
            onChange={e => setNewPass(e.target.value)}
            className={inputCls}
            placeholder="Min. 8 characters"
            autoComplete="new-password"
          />
        </Field>
        <Field label="Confirm new password">
          <input
            type="password"
            value={confirm}
            onChange={e => setConfirm(e.target.value)}
            className={inputCls}
            placeholder="Repeat new password"
            autoComplete="new-password"
          />
        </Field>
        {error && <p className="text-sm text-red-600">{error}</p>}
        {success && <p className="text-sm text-green-600">{success}</p>}
        <button type="submit" disabled={loading} className={btnPrimary}>
          {loading ? 'Saving…' : 'Update password'}
        </button>
      </form>
    </Section>
  )
}

// ---------------------------------------------------------------------------
// Main page
// ---------------------------------------------------------------------------

export default function Settings() {
  const [currentUser, setCurrentUser] = useState<CurrentUser | null>(null)

  useEffect(() => {
    fetch('/api/v1/users/me')
      .then(r => r.ok ? r.json() : null)
      .then(data => { if (data) setCurrentUser(data) })
      .catch(() => {})
  }, [])

  return (
    <div className="max-w-2xl mx-auto py-8 px-4 space-y-6">
      <h1 className="text-2xl font-bold text-gray-900">Settings</h1>

      {/* ── Ozma Connect ──────────────────────────────────────────── */}
      <ConnectSection />

      {/* ── Account / password (only when auth is enabled) ────────── */}
      {currentUser?.auth_enabled && (
        <ChangePasswordSection user={currentUser} />
      )}

      {/* ── Controller info ───────────────────────────────────────── */}
      <Section title="Controller">
        <div className="text-sm text-gray-600 space-y-2">
          <p>
            API docs:{' '}
            <a href="/docs" target="_blank" rel="noopener noreferrer" className="text-indigo-600 hover:underline">
              /docs
            </a>
          </p>
          <p>
            Prometheus metrics:{' '}
            <a href="/metrics" target="_blank" rel="noopener noreferrer" className="text-indigo-600 hover:underline">
              /metrics
            </a>
          </p>
          <p>
            GraphQL:{' '}
            <a href="/graphql" target="_blank" rel="noopener noreferrer" className="text-indigo-600 hover:underline">
              /graphql
            </a>
          </p>
        </div>
      </Section>
    </div>
  )
}
