import { createBrowserRouter, Navigate } from 'react-router-dom'
import Layout from '../components/Layout'
import NodesPage from '../pages/NodesPage'
import NodeDetailPage from '../pages/NodeDetailPage'

const router = createBrowserRouter([
  {
    path: '/',
    element: <Layout><NodesPage /></Layout>,
  },
  {
    path: '/nodes/:id',
    element: <Layout><NodeDetailPage /></Layout>,
  },
  {
    path: '*',
    element: <Navigate to="/" replace />,
  },
])

export { router }
