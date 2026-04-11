import { useEffect, useState, useCallback } from 'react'
import { useQuery, useMutation, useSubscription } from 'urql'
import { client } from '../graphql/client'
import {
  GET_NODES,
  SUBSCRIBE_NODE_STATE,
  GET_ACTIVE_NODE,
  SWITCH_NODE,
  SUBSCRIBE_NODE_STATUS_CHANGED
} from '../graphql/queries'
import { NodeInfo } from '../types/node'

// Custom hook for fetching nodes with GraphQL query
export function useNodesQuery() {
  const [result, setResult] = useState({ data: null as any, error: null as Error | null })

  useEffect(() => {
    const subscription = client
      .query(GET_NODES)
      .toPromise()
      .then((response) => {
        setResult({ data: response.data, error: response.error || null })
      })
      .catch((error) => {
        setResult({ data: null, error })
      })

    return () => {
      subscription.unsubscribe()
    }
  }, [])

  return result
}

// Custom hook for real-time node subscription (nodeStatusChanged events)
export function useNodeUpdates() {
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [error, setError] = useState<Error | null>(null)
  const [connected, setConnected] = useState(true)

  useEffect(() => {
    const observable = client.subscription(SUBSCRIBE_NODE_STATE)

    const subscription = observable.subscribe({
      next: (data: any) => {
        if (data.nodeStateChanged) {
          const node = data.nodeStateChanged
          setNodes((prev) => {
            // Check if node already exists
            const existing = prev.find(n => n.id === node.id)
            if (existing) {
              // Update existing node
              return prev.map(n => n.id === node.id ? { ...n, ...node } : n)
            }
            // Add new node
            return [...prev, node]
          })
        }
      },
      error: (err: Error) => {
        setError(err)
        setConnected(false)
      },
      complete: () => {
        setConnected(false)
      },
    })

    return () => {
      subscription.unsubscribe()
    }
  }, [])

  return { nodes, error, connected }
}

// Custom hook for getting active node
export function useActiveNode() {
  const [result, setResult] = useState({ data: null as any, error: null as Error | null, loading: true })

  useEffect(() => {
    const subscription = client
      .query(GET_ACTIVE_NODE)
      .toPromise()
      .then((response) => {
        setResult({
          data: response.data,
          error: response.error || null,
          loading: false
        })
      })
      .catch((error) => {
        setResult({ data: null, error, loading: false })
      })

    return () => {
      subscription.unsubscribe()
    }
  }, [])

  return result
}

// Custom hook for switching nodes (quick-switch)
export function useSwitchNode() {
  const [result, setResult] = useState({ data: null as any, error: null as Error | null, loading: false })

  const switchNode = useCallback(async (nodeId: string) => {
    setResult({ data: null, error: null, loading: true })

    try {
      const mutationResult = await client
        .mutation(SWITCH_NODE, { nodeId })
        .toPromise()

      setResult({
        data: mutationResult.data,
        error: mutationResult.error || null,
        loading: false
      })
    } catch (error) {
      setResult({ data: null, error: error as Error, loading: false })
    }
  }, [])

  return { switchNode, result }
}
