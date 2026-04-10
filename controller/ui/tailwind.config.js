import colors from 'tailwindcss/colors'

/** @type {import('tailwindcss').Config} */
export default {
  content: [
    './index.html',
    './src/**/*.{js,ts,jsx,tsx}',
  ],
  darkMode: 'class',
  theme: {
    extend: {
      colors: {
        emerald: {
          500: '#4ae0a4',
          400: '#6ee7b7',
          600: '#22c55e',
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'system-ui', 'sans-serif'],
      },
    },
  },
  plugins: [],
}
