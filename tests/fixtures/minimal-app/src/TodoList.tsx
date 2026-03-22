import { useState } from 'react';

import { useCustomHook } from './useCustomHook';

interface Todo {
  id: number;
  text: string;
  done: boolean;
}

export function TodoList() {
  const [todos, setTodos] = useState<Todo[]>([]);
  const { inputValue, setInputValue, reset } = useCustomHook('');

  const addTodo = () => {
    if (!inputValue.trim()) return;
    setTodos(prev => [...prev, { id: Date.now(), text: inputValue, done: false }]);
    reset();
  };

  const toggleTodo = (id: number) => {
    setTodos(prev => prev.map(t => (t.id === id ? { ...t, done: !t.done } : t)));
  };

  const remaining = todos.filter(t => !t.done).length;

  return (
    <div>
      <h2>Todos ({remaining} remaining)</h2>
      <input value={inputValue} onChange={e => setInputValue(e.target.value)} placeholder='Add todo...' />
      <button onClick={addTodo}>Add</button>
      <ul>
        {todos.map(todo => (
          <li
            key={todo.id}
            onClick={() => {
              toggleTodo(todo.id);
            }}
            style={{ textDecoration: todo.done ? 'line-through' : 'none' }}
          >
            {todo.text}
          </li>
        ))}
      </ul>
    </div>
  );
}
