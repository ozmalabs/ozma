import React, { FormEvent, useState } from 'react';
import { useAuth } from '../auth/AuthContext';
import { useSettings } from '../settings/useSettings';
import { useConnect } from '../settings/useConnect';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const MACHINE_CLASSES = ['workstation', 'server', 'kiosk', 'camera'] as const;
const THEMES = ['dark', 'light', 'system'] as const;

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section style={s.section}>
      <h2 style={s.sectionTitle}>{title}</h2>
      {children}
    </section>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <label style={s.field}>
      <span style={s.fieldLabel}>{label}</span>
      {children}
    </label>
  );
}

function SaveBar({
  saving,
  saved,
  error,
  onSave,
}: {
  saving: boolean;
  saved: boolean;
  error: string | null;
  onSave: () => void;
}) {
  return (
    <div style={s.saveBar}>
      {error && <span style={s.errorText}>{error}</span>}
      {saved && !error && <span style={s.savedText}>✓ Saved</span>}
      <button style={s.primaryBtn} onClick={onSave} disabled={saving}>
        {saving ? 'Saving…' : 'Save changes'}
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Connect section
// ---------------------------------------------------------------------------

function ConnectSection() {
  const { status, loading, working, error, link, unlink } = useConnect();
  const [linkToken, setLinkToken] = useState('');
  const [showLinkForm, setShowLinkForm] = useState(false);

  async function handleLink(e: FormEvent) {
    e.preventDefault();
    await link(linkToken.trim());
    setLinkToken('');
    setShowLinkForm(false);
  }

  if (loading) return <p style={s.muted}>Loading Connect status…</p>;

  return (
    <Section title="Ozma Connect">
      {error && <p style={s.errorText}>{error}</p>}

      {status?.linked ? (
        <div style={s.connectLinked}>
          <div style={s.connectRow}>
            <span style={s.connectedDot} />
            <span style={s.connectEmail}>
              {status.account_email ?? status.account_id ?? 'Linked account'}
            </span>
          </div>
          {status.last_seen && (
            <p style={s.muted}>
              Last seen: {new Date(status.last_seen).toLocaleString()}
            </p>
          )}
          <button
            style={s.dangerBtn}
            onClick={unlink}
            disabled={working}
          >
            {working ? 'Unlinking…' : 'Unlink account'}
          </button>
        </div>
      ) : (
        <div>
          <p style={s.muted}>
            Link this controller to an Ozma Connect account to enable remote
            access, push notifications, and cloud backup.
          </p>
          {showLinkForm ? (
            <form onSubmit={handleLink} style={s.linkForm}>
              <input
                style={s.input}
                type="text"
                placeholder="Paste your Connect link token"
                value={linkToken}
                onChange={(e) => setLinkToken(e.target.value)}
                required
              />
              <div style={s.linkFormBtns}>
                <button style={s.primaryBtn} type="submit" disabled={working}>
                  {working ? 'Linking…' : 'Link'}
                </button>
                <button
                  style={s.ghostBtn}
                  type="button"
                  onClick={() => setShowLinkForm(false)}
                >
                  Cancel
                </button>
              </div>
            </form>
          ) : (
            <button style={s.primaryBtn} onClick={() => setShowLinkForm(true)}>
              Link account
            </button>
          )}
        </div>
      )}
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Change-password section
// ---------------------------------------------------------------------------

function ChangePasswordSection() {
  const { user } = useAuth();
  const [current, setCurrent] = useState('');
  const [next, setNext] = useState('');
  const [confirm, setConfirm] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);
  const [loading, setLoading] = useState(false);

  if (!user?.auth_enabled) return null;

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    setSuccess(false);
    if (next !== confirm) {
      setError('New passwords do not match');
      return;
    }
    setLoading(true);
    try {
      const res = await fetch('/api/v1/users/me/password', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ current_password: current, new_password: next }),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error(body?.detail ?? body?.error ?? `HTTP ${res.status}`);
      }
      setSuccess(true);
      setCurrent('');
      setNext('');
      setConfirm('');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to change password');
    } finally {
      setLoading(false);
    }
  }

  return (
    <Section title="Change password">
      <form onSubmit={handleSubmit} style={s.form}>
        <Field label="Current password">
          <input
            style={s.input}
            type="password"
            autoComplete="current-password"
            value={current}
            onChange={(e) => setCurrent(e.target.value)}
            required
          />
        </Field>
        <Field label="New password">
          <input
            style={s.input}
            type="password"
            autoComplete="new-password"
            value={next}
            onChange={(e) => setNext(e.target.value)}
            required
          />
        </Field>
        <Field label="Confirm new password">
          <input
            style={s.input}
            type="password"
            autoComplete="new-password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            required
          />
        </Field>
        {error && <p style={s.errorText}>{error}</p>}
        {success && <p style={s.savedText}>✓ Password updated</p>}
        <button style={s.primaryBtn} type="submit" disabled={loading}>
          {loading ? 'Updating…' : 'Update password'}
        </button>
      </form>
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Main page
// ---------------------------------------------------------------------------

export default function Settings() {
  const { settings, setSettings, loading, saving, saved, error, save } =
    useSettings();

  function handleSave() {
    save(settings);
  }

  if (loading) {
    return <div style={s.page}><p style={s.muted}>Loading settings…</p></div>;
  }

  return (
    <div style={s.page}>
      <h1 style={s.pageTitle}>Settings</h1>

      {/* ── Controller config ─────────────────────────────────────── */}
      <Section title="Controller">
        <Field label="Node name">
          <input
            style={s.input}
            type="text"
            value={settings.node_name}
            onChange={(e) =>
              setSettings((p) => ({ ...p, node_name: e.target.value }))
            }
            placeholder="my-controller"
          />
        </Field>
        <Field label="Machine class">
          <select
            style={s.select}
            value={settings.machine_class}
            onChange={(e) =>
              setSettings((p) => ({
                ...p,
                machine_class: e.target.value as typeof settings.machine_class,
              }))
            }
          >
            {MACHINE_CLASSES.map((c) => (
              <option key={c} value={c}>
                {c.charAt(0).toUpperCase() + c.slice(1)}
              </option>
            ))}
          </select>
        </Field>
      </Section>

      {/* ── Virtual display defaults ───────────────────────────────── */}
      <Section title="Default virtual display">
        <div style={s.row}>
          <Field label="Width (px)">
            <input
              style={{ ...s.input, width: '6rem' }}
              type="number"
              min={640}
              max={7680}
              value={settings.display_width}
              onChange={(e) =>
                setSettings((p) => ({
                  ...p,
                  display_width: Number(e.target.value),
                }))
              }
            />
          </Field>
          <Field label="Height (px)">
            <input
              style={{ ...s.input, width: '6rem' }}
              type="number"
              min={480}
              max={4320}
              value={settings.display_height}
              onChange={(e) =>
                setSettings((p) => ({
                  ...p,
                  display_height: Number(e.target.value),
                }))
              }
            />
          </Field>
          <Field label="Refresh (Hz)">
            <input
              style={{ ...s.input, width: '5rem' }}
              type="number"
              min={24}
              max={360}
              value={settings.display_refresh}
              onChange={(e) =>
                setSettings((p) => ({
                  ...p,
                  display_refresh: Number(e.target.value),
                }))
              }
            />
          </Field>
        </div>
      </Section>

      {/* ── Preferences ───────────────────────────────────────────── */}
      <Section title="Preferences">
        <Field label="Theme">
          <select
            style={s.select}
            value={settings.theme}
            onChange={(e) =>
              setSettings((p) => ({
                ...p,
                theme: e.target.value as typeof settings.theme,
              }))
            }
          >
            {THEMES.map((t) => (
              <option key={t} value={t}>
                {t.charAt(0).toUpperCase() + t.slice(1)}
              </option>
            ))}
          </select>
        </Field>
      </Section>

      <SaveBar saving={saving} saved={saved} error={error} onSave={handleSave} />

      {/* ── Ozma Connect ──────────────────────────────────────────── */}
      <ConnectSection />

      {/* ── Account / password ────────────────────────────────────── */}
      <ChangePasswordSection />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

const s: Record<string, React.CSSProperties> = {
  page: {
    padding: '1.5rem 2rem',
    maxWidth: '640px',
    color: '#e2e8f0',
  },
  pageTitle: {
    margin: '0 0 1.5rem',
    fontSize: '1.5rem',
    fontWeight: 700,
    color: '#fff',
  },
  section: {
    marginBottom: '2rem',
    padding: '1.25rem 1.5rem',
    background: '#1a1d27',
    border: '1px solid #2a2d3a',
    borderRadius: '0.75rem',
    display: 'flex',
    flexDirection: 'column',
    gap: '1rem',
  },
  sectionTitle: {
    margin: 0,
    fontSize: '1rem',
    fontWeight: 600,
    color: '#94a3b8',
    textTransform: 'uppercase',
    letterSpacing: '0.05em',
  },
  field: {
    display: 'flex',
    flexDirection: 'column',
    gap: '0.375rem',
    fontSize: '0.875rem',
    color: '#94a3b8',
  },
  fieldLabel: {
    fontSize: '0.8125rem',
    color: '#94a3b8',
  },
  input: {
    padding: '0.5rem 0.75rem',
    borderRadius: '0.375rem',
    border: '1px solid #2a2d3a',
    background: '#0f1117',
    color: '#e2e8f0',
    fontSize: '0.9375rem',
    outline: 'none',
  },
  select: {
    padding: '0.5rem 0.75rem',
    borderRadius: '0.375rem',
    border: '1px solid #2a2d3a',
    background: '#0f1117',
    color: '#e2e8f0',
    fontSize: '0.9375rem',
    outline: 'none',
    cursor: 'pointer',
  },
  row: {
    display: 'flex',
    gap: '1rem',
    flexWrap: 'wrap',
  },
  form: {
    display: 'flex',
    flexDirection: 'column',
    gap: '0.875rem',
  },
  saveBar: {
    display: 'flex',
    alignItems: 'center',
    gap: '1rem',
    marginBottom: '2rem',
  },
  primaryBtn: {
    padding: '0.5rem 1.25rem',
    borderRadius: '0.375rem',
    border: 'none',
    background: '#6366f1',
    color: '#fff',
    fontSize: '0.9375rem',
    fontWeight: 600,
    cursor: 'pointer',
  },
  ghostBtn: {
    padding: '0.5rem 1rem',
    borderRadius: '0.375rem',
    border: '1px solid #2a2d3a',
    background: 'transparent',
    color: '#94a3b8',
    fontSize: '0.9375rem',
    cursor: 'pointer',
  },
  dangerBtn: {
    padding: '0.5rem 1rem',
    borderRadius: '0.375rem',
    border: '1px solid #7f1d1d',
    background: 'transparent',
    color: '#f87171',
    fontSize: '0.9375rem',
    cursor: 'pointer',
    alignSelf: 'flex-start',
  },
  errorText: {
    margin: 0,
    color: '#f87171',
    fontSize: '0.875rem',
  },
  savedText: {
    margin: 0,
    color: '#4ade80',
    fontSize: '0.875rem',
  },
  muted: {
    margin: 0,
    color: '#64748b',
    fontSize: '0.875rem',
  },
  connectLinked: {
    display: 'flex',
    flexDirection: 'column',
    gap: '0.5rem',
  },
  connectRow: {
    display: 'flex',
    alignItems: 'center',
    gap: '0.5rem',
  },
  connectedDot: {
    width: '0.5rem',
    height: '0.5rem',
    borderRadius: '50%',
    background: '#4ade80',
    flexShrink: 0,
  },
  connectEmail: {
    color: '#e2e8f0',
    fontSize: '0.9375rem',
  },
  linkForm: {
    display: 'flex',
    flexDirection: 'column',
    gap: '0.75rem',
  },
  linkFormBtns: {
    display: 'flex',
    gap: '0.75rem',
  },
};
