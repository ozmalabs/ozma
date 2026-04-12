interface Column<T> {
  key: keyof T | string;
  label: string;
  render?: (row: T) => React.ReactNode;
}

interface TableProps<T> {
  columns: Column<T>[];
  rows: T[];
  className?: string;
}

import React from 'react';

export default function Table<T extends { id?: string | number }>({
  columns,
  rows,
  className = '',
}: TableProps<T>) {
  return (
    <div className={`overflow-x-auto rounded-lg ${className}`}>
      <table className="w-full text-sm text-left">
        <thead className="bg-zinc-700 text-zinc-300 uppercase text-xs">
          <tr>
            {columns.map((col) => (
              <th key={String(col.key)} className="px-4 py-3 font-medium">
                {col.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, i) => (
            <tr
              key={row.id ?? i}
              className={i % 2 === 0 ? 'bg-zinc-800' : 'bg-zinc-800/60'}
            >
              {columns.map((col) => (
                <td key={String(col.key)} className="px-4 py-3 text-zinc-200">
                  {col.render
                    ? col.render(row)
                    : String((row as Record<string, unknown>)[col.key as string] ?? '')}
                </td>
              ))}
            </tr>
          ))}
          {rows.length === 0 && (
            <tr>
              <td
                colSpan={columns.length}
                className="px-4 py-6 text-center text-zinc-500"
              >
                No data.
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}
