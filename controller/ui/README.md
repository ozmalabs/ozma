# Ozma UI

React + Vite frontend for the Ozma KVMA router.

## Prerequisites

- Node.js 18+
- npm or pnpm

## Installation

```bash
cd controller/ui
npm install
```

## Development

```bash
npm run dev
```

The frontend will start on `http://localhost:5173` and proxy API requests to `http://localhost:7380`.

## Build

```bash
npm run build
```

The build output will be placed in `controller/static/ui/`.

## Scripts

- `npm run dev` - Start dev server with hot reload
- `npm run build` - Build for production
- `npm run lint` - Run ESLint
- `npm run preview` - Preview production build
