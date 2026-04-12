import { useCallback, useEffect, useState } from 'react';
import { getToken } from '../auth/tokenStorage';

export interface ConnectStatus {
  linked: boolean;
  account_email: string | null;
  account_id: string | null;
  last_seen: string | null;
}

function authHeaders(): HeadersInit {
  const token = getToken();
  return token
    ? { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` }
    : { 'Content-Type': 'application/json' };
}

export function useConnect() {
  const [status, setStatus] = useState<ConnectStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await fetch('/api/v1/connect/status', { headers: authHeaders() });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      setStatus(await res.json());
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load Connect status');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { load(); }, [load]);

  const link = useCallback(async (token: string) => {
    setWorking(true);
    setError(null);
    try {
      const res = await fetch('/api/v1/connect/link', {
        method: 'POST',
        headers: authHeaders(),
        body: JSON.stringify({ token }),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error(body?.detail ?? body?.error ?? `HTTP ${res.status}`);
      }
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Link failed');
    } finally {
      setWorking(false);
    }
  }, [load]);

  const unlink = useCallback(async () => {
    setWorking(true);
    setError(null);
    try {
      const res = await fetch('/api/v1/connect/unlink', {
        method: 'POST',
        headers: authHeaders(),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error(body?.detail ?? body?.error ?? `HTTP ${res.status}`);
      }
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unlink failed');
    } finally {
      setWorking(false);
    }
  }, [load]);

  return { status, loading, working, error, link, unlink, reload: load };
}
