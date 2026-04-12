import { ReactNode } from 'react';

interface CardProps {
  title?: string;
  children: ReactNode;
  className?: string;
}

export default function Card({ title, children, className = '' }: CardProps) {
  return (
    <div className={`bg-zinc-800 rounded-lg p-4 ${className}`}>
      {title && (
        <h2 className="text-zinc-100 font-semibold text-sm mb-3">{title}</h2>
      )}
      {children}
    </div>
  );
}
