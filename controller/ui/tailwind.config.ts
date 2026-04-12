import type { Config } from 'tailwindcss'

export default {
  darkMode: 'class',
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        'oz-bg':      '#18181b',
        'oz-surface': '#27272a',
        'oz-border':  '#3f3f46',
        'oz-muted':   '#71717a',
        'oz-text':    '#f4f4f5',
        'oz-primary': '#34d399',
        'oz-warning': '#fbbf24',
        'oz-error':   '#f87171',
      },
      fontFamily: {
        mono: ['"JetBrains Mono"', 'ui-monospace', 'SFMono-Regular', 'monospace'],
      },
    },
  },
  plugins: [],
} satisfies Config
