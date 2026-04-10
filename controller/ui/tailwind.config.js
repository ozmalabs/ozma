/** @type {import('tailwindcss').Config} */
export default {
  content: [
    './index.html',
    './src/**/*.{js,ts,jsx,tsx}',
  ],
  theme: {
    extend: {
      colors: {
        'bg': {
          'primary': 'var(--bg-primary)',
          'secondary': 'var(--bg-secondary)',
          'tertiary': 'var(--bg-tertiary)',
          'surface': 'var(--bg-surface)',
        },
        'text': {
          'primary': 'var(--text-primary)',
          'secondary': 'var(--text-secondary)',
          'muted': 'var(--text-muted)',
        },
        'border-color': 'var(--border-color)',
        'accent': {
          'emerald': 'var(--accent-emerald)',
          'emerald-dim': 'var(--accent-emerald-dim)',
          'emerald-light': 'var(--accent-emerald-light)',
          'danger': 'var(--accent-danger)',
          'warning': 'var(--accent-warning)',
          'info': 'var(--accent-info)',
        },
      },
    },
  },
  plugins: [],
}
