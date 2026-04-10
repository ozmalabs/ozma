/** @type {import('tailwindcss').Config} */
export default {
  content: [
    './index.html',
    './src/**/*.{js,ts,jsx,tsx}',
  ],
  theme: {
    extend: {
      colors: {
        emerald: {
          DEFAULT: '#4ae0a4',
          dim: '#2db385',
          light: '#7ee8c5',
        },
        bg: 'var(--color-bg)',
        'bg-secondary': 'var(--color-bg-secondary)',
        'bg-tertiary': 'var(--color-bg-tertiary)',
        text: 'var(--color-text)',
        'text-secondary': 'var(--color-text-secondary)',
      },
    },
  },
  plugins: [],
}
