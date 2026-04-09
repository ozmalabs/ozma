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
          50: '#e0f2f0',
          100: '#ccedd9',
          200: '#b2e0bc',
          300: '#94d59d',
          400: '#76c87e',
          500: '#4ae0a4',
          600: '#3ecb94',
          700: '#32b082',
          800: '#28926e',
          900: '#1e745a',
          950: '#0f3a2d',
        },
      },
    },
  },
  plugins: [],
}
