import { useEffect, useState } from 'react'
import { useQuery, useSubscription } from 'urql'
import { client } from '../graphql/client'
import { GET_NODES, SUBSCRIBE_NODE_STATE, GET_ACTIVE_NODE, ACTIVATE_NODE } from '../graphql/queries'
import { NodeInfo } from '../types/node'

// Custom hook for fetching nodes with GraphQL
export function useNodesQuery() {
  const [result, setResult] = useState({ data: null, error: null })

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

// Custom hook for node subscription
export function useNodeSubscription() {
  const [result, setResult] = useState({ data: null, error: null })

  useEffect(() => {
    const subscription = client
      .subscription(SUBSCRIBE_NODE_STATE)
      .toPromise()
      .then((response) => {
        if (response.data) {
          setResult({ data: response.data, error: null })
        }
        if (response.error) {
          setResult({ data: null, error: response.error })
        }
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

// Custom hook for getting active node
export function useActiveNode() {
  const [result, setResult] = useState({ data: null, error: null, loading: true })

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

// Custom hook for switching nodes
export function useSwitchNode() {
  const [result, setResult] = useState({ data: null, error: null, loading: false })

  const switchNode = async (nodeId: string) => {
    setResult({ data: null, error: null, loading: true })

    try {
      const mutationResult = await client
        .mutation(ACTIVATE_NODE, { nodeId })
        .toPromise()

      setResult({
        data: mutationResult.data,
        error: mutationResult.error || null,
        loading: false
      })
    } catch (error) {
      setResult({ data: null, error, loading: false })
    }
  }

  return { switchNode, result }
}

// Custom hook for real-time node updates
export function useNodeUpdates() {
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [error, setError] = useState<Error | null>(null)
  const [connected, setConnected] = useState(true)

  useEffect(() => {
    const observable = client.subscription(SUBSCRIBE_NODE_STATE)

    const subscription = observable.subscribe({
      next: (data) => {
        if (data.node) {
          setNodes((prev) => {
            const existing = prev.find(n => n.id === data.node.id)
            if (existing) {
              return prev.map(n => n.id === data.node.id ? data.node : n)
            }
            return [...prev, data.node]
          })
        }
      },
      error: (err) => {
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
