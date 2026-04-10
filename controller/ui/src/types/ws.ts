import { NodeInfo } from './api'

export type WebSocketEvent = NodeUpdateEvent | SystemEvent

export interface NodeUpdateEvent {
  type: 'node_update'
  payload: {
    node: NodeInfo
  }
}

export interface SystemEvent {
  type: 'system_update'
  payload: {
    active_node_id: string
    timestamp: string
  }
}
