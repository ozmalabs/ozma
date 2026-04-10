import { createBrowserRouter } from 'react-router-dom'
import { App } from '../App'
import { NodesPage } from '../pages/NodesPage'

export const router = createBrowserRouter([
  {
    path: '/',
    element: <App />,
    children: [
      {
        index: true,
        element: <NodesPage />,
      },
      {
        path: 'nodes',
        element: <NodesPage />,
      },
      {
        path: 'scenarios',
        element: <div>Scenarios page coming soon</div>,
      },
      {
        path: 'stream',
        element: <div>Stream page coming soon</div>,
      },
      {
        path: 'settings',
        element: <div>Settings page coming soon</div>,
      },
    ],
  },
])
