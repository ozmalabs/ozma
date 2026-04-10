/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        emerald: {
          50: '#e6fff0',
          100: '#ccffdd',
          200: '#b3ffcc',
          300: '#99ffba',
          400: '#80ffaa',
          500: '#66ff99',
          600: '#4ae0a4',
          700: '#3db88c',
          800: '#319474',
          900: '#267a60',
        },
        bg: {
          DEFAULT: '#0f1115',
          surface: '#1a1d23',
          'surface-light': '#252830',
        },
        text: {
          DEFAULT: '#e8e8e8',
          secondary: '#a0a0a0',
        },
        border: '#2d3038',
        accent: '#4ae0a4',
      },
    },
  },
  plugins: [],
}
