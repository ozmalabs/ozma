import { useState, useEffect, useCallback } from 'react'
import type { Scenario, ScenarioCreateRequest, ScenarioUpdateRequest } from '../types/node'

const API_BASE = '/api/v1'

export function useScenarios() {
  const [scenarios, setScenarios] = useState<Scenario[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const fetchScenarios = useCallback(async () => {
    try {
      setLoading(true)
      setError(null)
      const res = await fetch(`${API_BASE}/scenarios`)
      if (!res.ok) throw new Error(`Failed to fetch scenarios: ${res.status}`)
      const data = await res.json()
      setScenarios(data.scenarios ?? data)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Unknown error')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { fetchScenarios() }, [fetchScenarios])

  const createScenario = useCallback(async (req: ScenarioCreateRequest): Promise<Scenario> => {
    const res = await fetch(`${API_BASE}/scenarios`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(req),
    })
    if (!res.ok) {
      const text = await res.text()
      throw new Error(`Failed to create scenario: ${res.status} ${text}`)
    }
    const created: Scenario = await res.json()
    setScenarios(prev => [...prev, created])
    return created
  }, [])

  const updateScenario = useCallback(async (id: string, req: ScenarioUpdateRequest): Promise<Scenario> => {
    const res = await fetch(`${API_BASE}/scenarios/${id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(req),
    })
    if (!res.ok) {
      const text = await res.text()
      throw new Error(`Failed to update scenario: ${res.status} ${text}`)
    }
    const updated: Scenario = await res.json()
    setScenarios(prev => prev.map(s => s.id === id ? updated : s))
    return updated
  }, [])

  const deleteScenario = useCallback(async (id: string): Promise<void> => {
    const res = await fetch(`${API_BASE}/scenarios/${id}`, { method: 'DELETE' })
    if (!res.ok) {
      const text = await res.text()
      throw new Error(`Failed to delete scenario: ${res.status} ${text}`)
    }
    setScenarios(prev => prev.filter(s => s.id !== id))
  }, [])

  const activateScenario = useCallback(async (id: string): Promise<void> => {
    const res = await fetch(`${API_BASE}/scenarios/${id}/activate`, { method: 'POST' })
    if (!res.ok) {
      const text = await res.text()
      throw new Error(`Failed to activate scenario: ${res.status} ${text}`)
    }
    setScenarios(prev => prev.map(s => ({ ...s, active: s.id === id })))
  }, [])

  return {
    scenarios,
    loading,
    error,
    refresh: fetchScenarios,
    createScenario,
    updateScenario,
    deleteScenario,
    activateScenario,
  }
}
