import { useCallback, useState } from 'react';

export function useCustomHook(initialValue: string) {
  const [inputValue, setInputValue] = useState(initialValue);

  const reset = useCallback(() => {
    setInputValue(initialValue);
  }, [initialValue]);

  return { inputValue, setInputValue, reset };
}
