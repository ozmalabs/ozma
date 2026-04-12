import React, { FormEvent, useState } from 'react';
import { useNavigate, useLocation } from 'react-router-dom';
import { useAuth } from './AuthContext';

export default function LoginPage() {
  const { login } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();

  const from: string =
    (location.state as { from?: { pathname: string } })?.from?.pathname ?? '/';

  const [password, setPassword] = useState('');
  const [username, setUsername] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    setLoading(true);
    try {
      await login(password, username);
      navigate(from, { replace: true });
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : 'Login failed');
    } finally {
      setLoading(false);
    }
  }

  return (
    <div style={styles.wrapper}>
      <form onSubmit={handleSubmit} style={styles.card}>
        <h1 style={styles.title}>Ozma Controller</h1>

        <label style={styles.label}>
          Username (optional)
          <input
            style={styles.input}
            type="text"
            autoComplete="username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            placeholder="admin"
          />
        </label>

        <label style={styles.label}>
          Password
          <input
            style={styles.input}
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </label>

        {error && <p style={styles.error}>{error}</p>}

        <button style={styles.button} type="submit" disabled={loading}>
          {loading ? 'Signing in…' : 'Sign in'}
        </button>
      </form>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Minimal inline styles — no external CSS dependency required.
// ---------------------------------------------------------------------------

const styles: Record<string, React.CSSProperties> = {
  wrapper: {
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'center',
    minHeight: '100vh',
    background: '#0f1117',
  },
  card: {
    display: 'flex',
    flexDirection: 'column',
    gap: '1rem',
    background: '#1a1d27',
    border: '1px solid #2a2d3a',
    borderRadius: '0.75rem',
    padding: '2rem',
    width: '100%',
    maxWidth: '360px',
    color: '#e2e8f0',
  },
  title: {
    margin: 0,
    fontSize: '1.5rem',
    fontWeight: 700,
    textAlign: 'center',
    color: '#fff',
  },
  label: {
    display: 'flex',
    flexDirection: 'column',
    gap: '0.375rem',
    fontSize: '0.875rem',
    color: '#94a3b8',
  },
  input: {
    padding: '0.5rem 0.75rem',
    borderRadius: '0.375rem',
    border: '1px solid #2a2d3a',
    background: '#0f1117',
    color: '#e2e8f0',
    fontSize: '1rem',
    outline: 'none',
  },
  error: {
    margin: 0,
    color: '#f87171',
    fontSize: '0.875rem',
    textAlign: 'center',
  },
  button: {
    padding: '0.625rem',
    borderRadius: '0.375rem',
    border: 'none',
    background: '#6366f1',
    color: '#fff',
    fontSize: '1rem',
    fontWeight: 600,
    cursor: 'pointer',
  },
};
