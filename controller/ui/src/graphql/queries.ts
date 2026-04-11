import { gql } from 'urql'

// GraphQL query for listing all nodes
export const GET_NODES = gql`
  query GetNodes {
    nodes {
      id
      name
      host
      port
      role
      hw
      fw_version
      proto_version
      capabilities
      machine_class
      last_seen
      status
      uptime_seconds
      ip_address
      mac_address
      hostname
      platform
      version
      vnc_host
      vnc_port
      stream_port
      stream_path
      audio_type
      audio_sink
      audio_vban_port
      mic_vban_port
      capture_device
      camera_streams {
        name
        rtsp_inbound
        backchannel
        hls
      }
      frigate_host
      frigate_port
      owner_user_id
      owner_id
      shared_with
      share_permissions
      parent_node_id
      sunshine_port
      seat_count
      seat_config
      display_outputs {
        index
        source_type
        capture_source_id
        width
        height
      }
      scenario {
        id
        name
        color
      }
      hid_stats {
        total_keys
        total_clicks
        total_scrolls
        last_activity
      }
    }
  }
`

// GraphQL query for getting a single node by ID
export const GET_NODE_BY_ID = gql`
  query GetNodeById($id: ID!) {
    node(id: $id) {
      id
      name
      host
      port
      role
      hw
      fw_version
      proto_version
      capabilities
      machine_class
      last_seen
      status
      uptime_seconds
      ip_address
      mac_address
      hostname
      platform
      version
      vnc_host
      vnc_port
      stream_port
      stream_path
      audio_type
      audio_sink
      audio_vban_port
      mic_vban_port
      capture_device
      camera_streams {
        name
        rtsp_inbound
        backchannel
        hls
      }
      frigate_host
      frigate_port
      owner_user_id
      owner_id
      shared_with
      share_permissions
      parent_node_id
      sunshine_port
      seat_count
      seat_config
      display_outputs {
        index
        source_type
        capture_source_id
        width
        height
      }
      scenario {
        id
        name
        color
      }
      hid_stats {
        total_keys
        total_clicks
        total_scrolls
        last_activity
      }
    }
  }
`

// GraphQL query for getting the active node
export const GET_ACTIVE_NODE = gql`
  query GetActiveNode {
    active_node {
      id
      name
      host
      port
      role
      hw
      fw_version
      proto_version
      capabilities
      machine_class
      last_seen
      status
      uptime_seconds
      ip_address
      mac_address
      hostname
      platform
      version
      vnc_host
      vnc_port
      stream_port
      stream_path
      audio_type
      audio_sink
      audio_vban_port
      mic_vban_port
      capture_device
      camera_streams {
        name
        rtsp_inbound
        backchannel
        hls
      }
      frigate_host
      frigate_port
      owner_user_id
      owner_id
      shared_with
      share_permissions
      parent_node_id
      sunshine_port
      seat_count
      seat_config
      display_outputs {
        index
        source_type
        capture_source_id
        width
        height
      }
      scenario {
        id
        name
        color
      }
      hid_stats {
        total_keys
        total_clicks
        total_scrolls
        last_activity
      }
    }
  }
`

// GraphQL mutation for activating a node (quick-switch)
export const SWITCH_NODE = gql`
  mutation SwitchNode($nodeId: ID!) {
    activate_node(node_id: $nodeId) {
      nodes {
        id
        name
        host
        port
        role
        hw
        fw_version
        proto_version
        capabilities
        machine_class
        last_seen
        status
        uptime_seconds
        ip_address
        mac_address
        hostname
        platform
        version
        vnc_host
        vnc_port
        stream_port
        stream_path
        audio_type
        audio_sink
        audio_vban_port
        mic_vban_port
        capture_device
        camera_streams {
          name
          rtsp_inbound
          backchannel
          hls
        }
        frigate_host
        frigate_port
        owner_user_id
        owner_id
        shared_with
        share_permissions
        parent_node_id
        sunshine_port
        seat_count
        seat_config
        display_outputs {
          index
          source_type
          capture_source_id
          width
          height
        }
        scenario {
          id
          name
          color
        }
        hid_stats {
          total_keys
          total_clicks
          total_scrolls
          last_activity
        }
      }
      active_node_id
    }
  }
`

// GraphQL subscription for node state changes
// This subscription receives real-time updates when nodes come online,
// go offline, or when the active node changes (nodeStatusChanged)
export const SUBSCRIBE_NODE_STATE = gql`
  subscription SubscribeNodeState {
    nodeStateChanged {
      id
      name
      host
      port
      role
      hw
      fw_version
      proto_version
      capabilities
      machine_class
      last_seen
      status
      uptime_seconds
      ip_address
      mac_address
      hostname
      platform
      version
      vnc_host
      vnc_port
      stream_port
      stream_path
      audio_type
      audio_sink
      audio_vban_port
      mic_vban_port
      capture_device
      camera_streams {
        name
        rtsp_inbound
        backchannel
        hls
      }
      frigate_host
      frigate_port
      owner_user_id
      owner_id
      shared_with
      share_permissions
      parent_node_id
      sunshine_port
      seat_count
      seat_config
      display_outputs {
        index
        source_type
        capture_source_id
        width
        height
      }
      scenario {
        id
        name
        color
      }
      hid_stats {
        total_keys
        total_clicks
        total_scrolls
        last_activity
      }
    }
  }
`

// GraphQL subscription for node status changes (nodeStatusChanged event)
// This is an alias for nodeStateChanged - both receive the same events
export const SUBSCRIBE_NODE_STATUS_CHANGED = gql`
  subscription SubscribeNodeStatusChanged {
    nodeStateChanged {
      id
      name
      host
      port
      role
      hw
      fw_version
      proto_version
      capabilities
      machine_class
      last_seen
      status
      uptime_seconds
      ip_address
      mac_address
      hostname
      platform
      version
      vnc_host
      vnc_port
      stream_port
      stream_path
      audio_type
      audio_sink
      audio_vban_port
      mic_vban_port
      capture_device
      camera_streams {
        name
        rtsp_inbound
        backchannel
        hls
      }
      frigate_host
      frigate_port
      owner_user_id
      owner_id
      shared_with
      share_permissions
      parent_node_id
      sunshine_port
      seat_count
      seat_config
      display_outputs {
        index
        source_type
        capture_source_id
        width
        height
      }
      scenario {
        id
        name
        color
      }
      hid_stats {
        total_keys
        total_clicks
        total_scrolls
        last_activity
      }
    }
  }
`

// GraphQL subscription for scenario changes
export const SUBSCRIBE_SCENARIO_STATE = gql`
  subscription SubscribeScenarioState {
    scenarioChanged {
      id
      name
      node_id
      color
      config
    }
  }
`

// GraphQL query for getting system snapshot
export const GET_SYSTEM_SNAPSHOT = gql`
  query GetSystemSnapshot {
    snapshot {
      nodes {
        id
        name
        host
        port
        role
        hw
        fw_version
        proto_version
        capabilities
        machine_class
        last_seen
        status
        uptime_seconds
        ip_address
        mac_address
        hostname
        platform
        version
        vnc_host
        vnc_port
        stream_port
        stream_path
        audio_type
        audio_sink
        audio_vban_port
        mic_vban_port
        capture_device
        camera_streams {
          name
          rtsp_inbound
          backchannel
          hls
        }
        frigate_host
        frigate_port
        owner_user_id
        owner_id
        shared_with
        share_permissions
        parent_node_id
        sunshine_port
        seat_count
        seat_config
        display_outputs {
          index
          source_type
          capture_source_id
          width
          height
        }
        scenario {
          id
          name
          color
        }
        hid_stats {
          total_keys
          total_clicks
          total_scrolls
          last_activity
        }
      }
      active_node_id
    }
  }
`
