import { useEffect, useState } from 'react';

import { ThemeProvider } from './Context';
import { Counter } from './Counter';
import { TodoList } from './TodoList';

export function App() {
  const [title, setTitle] = useState('React Compiler Test');

  useEffect(() => {
    document.title = title;
  }, [title]);

  return (
    <ThemeProvider>
      <div>
        <h1>{title}</h1>
        <input value={title} onChange={e => setTitle(e.target.value)} />
        <Counter />
        <TodoList />
      </div>
    </ThemeProvider>
  );
}
