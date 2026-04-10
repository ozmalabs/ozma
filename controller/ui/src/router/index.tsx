import React from 'react';
import { createBrowserRouter, RouterProvider } from 'react-router-dom';
import Layout from '../components/layout/Layout';
import NodesPage from '../pages/NodesPage';

const router = createBrowserRouter([
  {
    path: '/',
    element: (
      <Layout>
        <NodesPage />
      </Layout>
    ),
    errorElement: (
      <Layout>
        <div className="flex items-center justify-center h-full">
          <div className="text-center">
            <h2 className="text-2xl font-bold text-gray-200 mb-2">Page Not Found</h2>
            <p className="text-gray-400">The page you are looking for does not exist.</p>
            <button
              onClick={() => window.location.href = '/'}
              className="mt-4 px-4 py-2 bg-emerald-600 hover:bg-emerald-700 text-white rounded-lg transition-colors"
            >
              Go Home
            </button>
          </div>
        </div>
      </Layout>
    ),
  },
]);

export const AppRouter: React.FC = () => {
  return <RouterProvider router={router} />;
};

export default AppRouter;
