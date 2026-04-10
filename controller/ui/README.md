# Ozma UI

React + Vite frontend for the Ozma Controller.

## Prerequisites

- Node.js 18+ and npm

## Installation

```bash
cd controller/ui
npm install
```

## Development

```bash
npm run dev
```

The UI will be available at `http://localhost:5173` with proxy to `http://localhost:7380`.

## Build

```bash
npm run build
```

Output will be in the `dist` directory.

## Project Structure

```
controller/ui/
├── src/
│   ├── layouts/
│   │   └── Layout.tsx      # Main layout with sidebar and topbar
│   ├── pages/
│   │   └── NodesPage.tsx   # Nodes list page
│   ├── store/
│   │   └── useNodesStore.ts # Zustand store for node state
│   ├── App.tsx             # Root app with routing
│   ├── main.tsx            # Entry point
│   └── index.css           # Global styles with Tailwind
├── vite.config.ts          # Vite configuration with API proxy
├── package.json
└── tailwind.config.js
```

## Tech Stack

- React 18
- TypeScript
- Vite
- TailwindCSS
- React Router DOM
- Zustand

## API Integration

The UI proxies `/api` requests to `http://localhost:7380` and uses REST fetch calls for data.
WebSocket connections are used for live updates.
