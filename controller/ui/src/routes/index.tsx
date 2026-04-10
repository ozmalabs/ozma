import { createBrowserRouter } from 'react-router-dom'
import App from '../App'
import NodesPage from '../pages/NodesPage'
import NodeDetailPage from '../pages/NodeDetailPage'

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
        path: '/nodes/:id',
        element: <NodeDetailPage />,
      },
    ],
  },
])

export default router
