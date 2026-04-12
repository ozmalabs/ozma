import { Outlet, useNavigate } from 'react-router-dom';
import Sidebar from '../components/Sidebar';
import TopBar from '../components/TopBar';

interface AppLayoutProps {
  activeNodeName?: string;
}

export default function AppLayout({ activeNodeName }: AppLayoutProps) {
  const navigate = useNavigate();

  function handleLogout() {
    // Clear any stored auth tokens
    localStorage.removeItem('token');
    navigate('/login');
  }

  return (
    <div className="flex h-screen bg-zinc-950 text-zinc-100 overflow-hidden">
      <Sidebar />
      <div className="flex flex-col flex-1 min-w-0">
        <TopBar activeNodeName={activeNodeName} onLogout={handleLogout} />
        <main className="flex-1 overflow-y-auto p-6">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
