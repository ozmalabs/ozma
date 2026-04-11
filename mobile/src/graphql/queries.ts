/**
 * GraphQL queries for NodeDetailScreen using urql.
 * Based on the GraphQL schema from /mnt/internal/implementation/graphql-schema-design.md
 */

import {gql} from '@urql/core';

// ── Node fragment for reusable query fields ─────────────────────────────────────────────────────

export const NODE_FRAGMENT = gql`
  fragment NodeFields on Node {
    id
    name
    host
    port
    role
    hw
    fwVersion
    protoVersion
    capabilities
    machineClass
    lastSeen
    displayOutputs {
      id
      name
      resolution
      connected
    }
    vncHost
    vncPort
    streamPort
    streamPath
    audioType
    audioSink
    audioVBANPort
    micVBANPort
    captureDevice
    cameraStreams {
      id
      name
      type
      url
    }
    frigateHost
    frigatePort
    ownerUserId
    owner
    sharedWith
    sharePermissions
    parentId
    sunshinePort
  }
`;

export const SCENARIO_FRAGMENT = gql`
  fragment ScenarioFields on Scenario {
    id
    name
    nodeId
    color
    transitionIn {
      style
      durationMs
    }
    motion {
      deviceId
      axis
      position
    }
    bluetooth {
      connect
      disconnect
    }
    captureSource
    captureSources
    wallpaper {
      mode
      color
      image
      url
    }
  }
`;

// ── Queries ─────────────────────────────────────────────────────────────────────────────────────

export const GET_NODE = gql`
  query GetNode($id: ID!) {
    node(id: $id) {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const GET_ACTIVE_NODE = gql`
  query GetActiveNode {
    activeNode {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const GET_NODE_LIST = gql`
  query GetNodeList {
    nodes {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const GET_SCENARIO = gql`
  query GetScenario($id: ID!) {
    scenario(id: $id) {
      ...ScenarioFields
    }
  }
  ${SCENARIO_FRAGMENT}
`;

// ── Mutations ───────────────────────────────────────────────────────────────────────────────────

export const ACTIVATE_NODE = gql`
  mutation ActivateNode($id: ID!) {
    activateNode(id: $id) {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const RENAME_NODE = gql`
  mutation RenameNode($id: ID!, $name: String!) {
    renameNode(id: $id, name: $name) {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const WAKE_ON_LAN = gql`
  mutation WakeOnLan($id: ID!) {
    wakeOnLan(id: $id) {
      success
      mac
      broadcast
      message
    }
  }
`;

export const ACTIVATE_SCENARIO = gql`
  mutation ActivateScenario($id: ID!) {
    activateScenario(id: $id) {
      ...ScenarioFields
    }
  }
  ${SCENARIO_FRAGMENT}
`;

// ── Subscriptions ───────────────────────────────────────────────────────────────────────────────

export const SUBSCRIBE_NODE_CHANGED = gql`
  subscription OnNodeChanged($id: ID!) {
    nodeChanged(id: $id) {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const SUBSCRIBE_NODE_ADDED = gql`
  subscription OnNodeAdded {
    nodeAdded {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const SUBSCRIBE_NODE_REMOVED = gql`
  subscription OnNodeRemoved($id: ID!) {
    nodeRemoved(id: $id) {
      ...NodeFields
    }
  }
  ${NODE_FRAGMENT}
`;

export const SUBSCRIBE_SCENARIO_CHANGED = gql`
  subscription OnScenarioChanged($id: ID!) {
    scenarioChanged(id: $id) {
      ...ScenarioFields
    }
  }
  ${SCENARIO_FRAGMENT}
`;

export const SUBSCRIBE_ACTIVE_NODE_CHANGED = gql`
  subscription OnActiveNodeChanged {
    nodeChanged(id: "") {
      id
    }
  }
`;
