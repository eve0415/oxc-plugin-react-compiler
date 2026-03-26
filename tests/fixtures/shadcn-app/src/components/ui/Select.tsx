import { createContext, useContext, useState, useCallback, useRef, useEffect, type ReactNode } from 'react';

import { cn } from '../../lib/utils';

interface SelectContextValue {
  value: string;
  onValueChange: (value: string) => void;
  open: boolean;
  setOpen: (open: boolean) => void;
}

const SelectContext = createContext<SelectContextValue | null>(null);

function useSelectContext(): SelectContextValue {
  const ctx = useContext(SelectContext);
  if (!ctx) throw new Error('Select components must be used within a Select');
  return ctx;
}

export function Select({ value: controlledValue, onValueChange, children }: { value?: string; onValueChange?: (value: string) => void; children: ReactNode }) {
  const [internalValue, setInternalValue] = useState('');
  const [open, setOpen] = useState(false);
  const value = controlledValue ?? internalValue;
  const handleChange = onValueChange ?? setInternalValue;

  return (
    <SelectContext.Provider value={{ value, onValueChange: handleChange, open, setOpen }}>
      <div className='relative'>{children}</div>
    </SelectContext.Provider>
  );
}

export function SelectTrigger({ children, className }: { children: ReactNode; className?: string }) {
  const { open, setOpen } = useSelectContext();
  const handleClick = useCallback(() => setOpen(!open), [open, setOpen]);

  return (
    <button
      className={cn('flex h-10 w-full items-center justify-between rounded-md border border-input', 'bg-background px-3 py-2 text-sm', className)}
      onClick={handleClick}
    >
      {children}
      <span className={cn('ml-2 transition-transform', open && 'rotate-180')}>▾</span>
    </button>
  );
}

export function SelectContent({ children, className }: { children: ReactNode; className?: string }) {
  const { open, setOpen } = useSelectContext();
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handleClickOutside = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, [open, setOpen]);

  if (!open) return null;

  return (
    <div ref={ref} className={cn('absolute z-50 mt-1 w-full rounded-md border bg-popover shadow-md', 'animate-in fade-in-0 zoom-in-95', className)}>
      {children}
    </div>
  );
}

export function SelectItem({ value, children, className }: { value: string; children: ReactNode; className?: string }) {
  const { value: selectedValue, onValueChange, setOpen } = useSelectContext();
  const isSelected = value === selectedValue;

  const handleClick = useCallback(() => {
    onValueChange(value);
    setOpen(false);
  }, [value, onValueChange, setOpen]);

  return (
    <div
      className={cn(
        'relative flex cursor-pointer select-none items-center rounded-sm px-2 py-1.5 text-sm',
        'hover:bg-accent hover:text-accent-foreground',
        isSelected && 'bg-accent',
        className,
      )}
      onClick={handleClick}
    >
      {isSelected && <span className='mr-2'>✓</span>}
      {children}
    </div>
  );
}
