import { gql } from 'urql'

// ============== NODE QUERIES ==============

// Get all nodes
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
        node_id
        transition_in {
          style
          duration_ms
        }
        motion {
          device_id
          axis
          position
        }
        bluetooth {
          connect
          disconnect
        }
        capture_source
        capture_sources
        wallpaper {
          mode
          color
          image
          url
        }
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

// Get a single node by ID
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
        node_id
        transition_in {
          style
          duration_ms
        }
        motion {
          device_id
          axis
          position
        }
        bluetooth {
          connect
          disconnect
        }
        capture_source
        capture_sources
        wallpaper {
          mode
          color
          image
          url
        }
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

// Get active node
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
        node_id
        transition_in {
          style
          duration_ms
        }
        motion {
          device_id
          axis
          position
        }
        bluetooth {
          connect
          disconnect
        }
        capture_source
        capture_sources
        wallpaper {
          mode
          color
          image
          url
        }
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

// ============== NODE MUTATIONS ==============

// Activate a node
export const ACTIVATE_NODE = gql`
  mutation ActivateNode($nodeId: ID!) {
    activate_node(id: $nodeId) {
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

// Rename a node
export const RENAME_NODE = gql`
  mutation RenameNode($id: ID!, $name: String!) {
    rename_node(id: $id, name: $name) {
      id
      name
    }
  }
`

// Wake on LAN
export const WAKE_ON_LAN = gql`
  mutation WakeOnLan($id: ID!) {
    wake_on_lan(id: $id) {
      success
      mac
      broadcast
      message
    }
  }
`

// ============== SCENARIO QUERIES ==============

// Get all scenarios
export const GET_SCENARIOS = gql`
  query GetScenarios {
    scenarios {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// Get a single scenario by ID
export const GET_SCENARIO_BY_ID = gql`
  query GetScenarioById($id: ID!) {
    scenario(id: $id) {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// Get active scenario
export const GET_ACTIVE_SCENARIO = gql`
  query GetActiveScenario {
    active_scenario {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// ============== SCENARIO MUTATIONS ==============

// Create a scenario
export const CREATE_SCENARIO = gql`
  mutation CreateScenario($input: ScenarioInput!) {
    create_scenario(input: $input) {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// Update a scenario
export const UPDATE_SCENARIO = gql`
  mutation UpdateScenario($id: ID!, $input: ScenarioInput!) {
    update_scenario(id: $id, input: $input) {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// Delete a scenario
export const DELETE_SCENARIO = gql`
  mutation DeleteScenario($id: ID!) {
    delete_scenario(id: $id) {
      success
      deleted_id
      message
    }
  }
`

// Activate a scenario
export const ACTIVATE_SCENARIO = gql`
  mutation ActivateScenario($id: ID!) {
    activate_scenario(id: $id) {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// ============== AUDIO MUTATIONS ==============

// Set audio volume
export const SET_AUDIO_VOLUME = gql`
  mutation SetAudioVolume($nodeId: ID!, $volume: Float!) {
    set_audio_volume(node_id: $nodeId, volume: $volume) {
      node_id
      volume
      muted
      audio_type
      audio_sink
      vban_port
    }
  }
`

// Mute/unmute a node
export const MUTE_NODE = gql`
  mutation MuteNode($nodeId: ID!, $muted: Boolean!) {
    mute_node(node_id: $nodeId, muted: $muted) {
      node_id
      muted
      audio_type
      audio_sink
      vban_port
    }
  }
`

// Set audio route
export const SET_AUDIO_ROUTE = gql`
  mutation SetAudioRoute($source: ID!, $target: ID!, $active: Boolean!) {
    set_audio_route(source: $source, target: $target, active: $active) {
      source_id
      target_id
      active
    }
  }
`

// ============== SUBSCRIPTIONS ==============

// Subscribe to node state changes (single node)
export const SUBSCRIBE_NODE_CHANGED = gql`
  subscription SubscribeNodeChanged($id: ID!) {
    node_changed(id: $id) {
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

// Subscribe to node added events
export const SUBSCRIBE_NODE_ADDED = gql`
  subscription SubscribeNodeAdded {
    node_added {
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

// Subscribe to node removed events
export const SUBSCRIBE_NODE_REMOVED = gql`
  subscription SubscribeNodeRemoved($id: ID!) {
    node_removed(id: $id) {
      id
    }
  }
`

// Subscribe to scenario changes
export const SUBSCRIBE_SCENARIO_CHANGED = gql`
  subscription SubscribeScenarioChanged($id: ID!) {
    scenario_changed(id: $id) {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// Subscribe to scenario added events
export const SUBSCRIBE_SCENARIO_ADDED = gql`
  subscription SubscribeScenarioAdded {
    scenario_added {
      id
      name
      node_id
      color
      transition_in {
        style
        duration_ms
      }
      motion {
        device_id
        axis
        position
      }
      bluetooth {
        connect
        disconnect
      }
      capture_source
      capture_sources
      wallpaper {
        mode
        color
        image
        url
      }
    }
  }
`

// Subscribe to scenario deleted events
export const SUBSCRIBE_SCENARIO_DELETED = gql`
  subscription SubscribeScenarioDeleted($id: ID!) {
    scenario_deleted(id: $id) {
      id
    }
  }
`

// Subscribe to audio changes
export const SUBSCRIBE_AUDIO_CHANGED = gql`
  subscription SubscribeAudioChanged($nodeId: ID!) {
    audio_changed(node_id: $nodeId) {
      node_id
      volume
      muted
      audio_type
      audio_sink
      vban_port
    }
  }
`

// ============== SYSTEM QUERIES ==============

// Get system info
export const GET_SYSTEM_INFO = gql`
  query GetSystemInfo {
    system_info {
      version
      active_node_id
      active_scenario_id
      node_count
      scenario_count
      audio_enabled
      auth_enabled
      uptime_seconds
    }
  }
`
