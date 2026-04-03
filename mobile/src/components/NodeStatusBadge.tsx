import React from 'react';
import {StyleSheet, Text, View} from 'react-native';

interface Props {
  online: boolean;
  /** If true, renders a larger, text-labelled badge. Default: compact dot only. */
  labeled?: boolean;
}

/**
 * Small status indicator for a node / machine.
 * online = green dot, offline = red dot.
 * Pass labeled=true for "Online" / "Offline" text alongside the dot.
 */
export function NodeStatusBadge({online, labeled = false}: Props) {
  const color = online ? styles.dotOnline : styles.dotOffline;
  const label = online ? 'Online' : 'Offline';

  return (
    <View style={styles.row}>
      <View style={[styles.dot, color]} />
      {labeled && (
        <Text style={[styles.label, online ? styles.textOnline : styles.textOffline]}>
          {label}
        </Text>
      )}
    </View>
  );
}

const styles = StyleSheet.create({
  row: {
    flexDirection: 'row',
    alignItems: 'center',
  },
  dot: {
    width: 10,
    height: 10,
    borderRadius: 5,
  },
  dotOnline: {
    backgroundColor: '#34D399', // emerald-400
  },
  dotOffline: {
    backgroundColor: '#F87171', // red-400
  },
  label: {
    marginLeft: 6,
    fontSize: 13,
    fontWeight: '500',
  },
  textOnline: {
    color: '#059669',
  },
  textOffline: {
    color: '#DC2626',
  },
});
