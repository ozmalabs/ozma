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
        },
      },
    },
  },
  plugins: [],
};
