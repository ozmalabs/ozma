import React from 'react';
import Sidebar from './Sidebar';
import Topbar from './Topbar';

interface LayoutProps {
  children: React.ReactNode;
}

const Layout: React.FC<LayoutProps> = ({ children }) => {
  const [sidebarOpen, setSidebarOpen] = React.useState(true);
  const [isDark, setIsDark] = React.useState(true);

  const toggleTheme = () => {
    setIsDark(!isDark);
  };

  React.useEffect(() => {
    if (isDark) {
      document.documentElement.classList.add('dark');
    } else {
      document.documentElement.classList.remove('dark');
    }
  }, [isDark]);

  return (
    <div className="flex h-screen bg-gray-950">
      <Sidebar isOpen={sidebarOpen} />
      <div className="flex flex-1 flex-col overflow-hidden">
        <Topbar
          isOpen={sidebarOpen}
          setIsOpen={setSidebarOpen}
          toggleTheme={toggleTheme}
          isDark={isDark}
        />
        <main className="flex-1 overflow-auto p-4">
          {children}
        </main>
      </div>
    </div>
  );
};

export default Layout;
