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
        primary: {
          50: '#e6f7f4',
          100: '#c7eee5',
          200: '#a3e4d4',
          300: '#7ed1bf',
          400: '#5ec6b1',
          500: '#4ae0a4',
          600: '#3dc692',
          700: '#32a57b',
          800: '#2b8b69',
          900: '#26755a',
        },
      },
    },
  },
  plugins: [],
}
