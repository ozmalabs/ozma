import { createBrowserRouter } from 'react-router-dom'
import App from '../App'
import NodesPage from '../pages/NodesPage'

const router = createBrowserRouter([
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
        path: 'nodes/:id',
        element: <div>Node detail page (placeholder)</div>,
      },
      {
        path: 'scenarios',
        element: <div>Scenarios page (placeholder)</div>,
      },
      {
        path: 'settings',
        element: <div>Settings page (placeholder)</div>,
      },
    ],
  },
])

export default router
