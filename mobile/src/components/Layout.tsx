/**
 * Layout component for consistent screen structure across the app.
 * Provides standardized header, padding, and container styling.
 */

import React from 'react';
import {
  SafeAreaView,
  ScrollView,
  StyleSheet,
  Text,
  View,
  ViewProps,
  ScrollViewProps,
} from 'react-native';
import {useSafeAreaInsets} from 'react-native-safe-area-context';

interface LayoutProps extends ViewProps {
  children: React.ReactNode;
  scrollable?: boolean;
  scrollProps?: ScrollViewProps;
  containerStyle?: ViewProps['style'];
}

/**
 * Layout — reusable screen container with consistent styling.
 * Wraps content in a SafeAreaView with standardized padding and background.
 */
export function Layout({
  children,
  scrollable = false,
  scrollProps = {},
  containerStyle,
  style,
  ...props
}: LayoutProps) {
  const insets = useSafeAreaInsets();

  const ContentWrapper = scrollable ? ScrollView : View;

  return (
    <SafeAreaView style={[styles.safeArea, {paddingTop: insets.top, paddingBottom: insets.bottom}]}>
      <ContentWrapper
        style={[styles.container, containerStyle]}
        contentContainerStyle={styles.contentContainer}
        {...(scrollable ? scrollProps : props)}
      >
        <View style={[styles.inner, style]} {...props}>
          {children}
        </View>
      </ContentWrapper>
    </SafeAreaView>
  );
}

/**
 * Section — standardized content section with header.
 * Used for grouping related content within a screen.
 */
interface SectionProps extends ViewProps {
  title?: string;
  children: React.ReactNode;
  compact?: boolean;
}

export function Section({title, children, compact = false, style, ...props}: SectionProps) {
  return (
    <View style={[styles.section, compact && styles.sectionCompact, style]} {...props}>
      {title && <View style={styles.sectionHeader}><Text style={styles.sectionTitle}>{title}</Text></View>}
      <View style={styles.sectionContent}>{children}</View>
    </View>
  );
}

// ── Styles ────────────────────────────────────────────────────────────────────

const styles = StyleSheet.create({
  safeArea: {
    flex: 1,
    backgroundColor: '#111827',
  },
  container: {
    flex: 1,
  },
  contentContainer: {
    padding: 16,
    gap: 16,
  },
  inner: {
    flex: 1,
  },
  section: {
    backgroundColor: '#1F2937',
    borderRadius: 12,
    padding: 16,
    gap: 12,
  },
  sectionCompact: {
    padding: 12,
    gap: 8,
  },
  sectionHeader: {
    gap: 4,
  },
  sectionTitle: {
    color: '#9CA3AF',
    fontSize: 12,
    fontWeight: '600',
    textTransform: 'uppercase',
    letterSpacing: 0.5,
  },
  sectionContent: {
    gap: 8,
  },
});
