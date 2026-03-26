import { createContext, useContext, useState, useCallback, useEffect, useRef, type ReactNode } from 'react';

import { cn } from '../../lib/utils';

interface DialogContextValue {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

const DialogContext = createContext<DialogContextValue | null>(null);

function useDialogContext(): DialogContextValue {
  const ctx = useContext(DialogContext);
  if (!ctx) throw new Error('Dialog components must be used within a Dialog');
  return ctx;
}

export function Dialog({
  open: controlledOpen,
  onOpenChange: controlledOnChange,
  children,
}: {
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  children: ReactNode;
}) {
  const [internalOpen, setInternalOpen] = useState(false);
  const open = controlledOpen ?? internalOpen;
  const onOpenChange = controlledOnChange ?? setInternalOpen;

  return <DialogContext.Provider value={{ open, onOpenChange }}>{children}</DialogContext.Provider>;
}

export function DialogTrigger({ children, asChild }: { children: ReactNode; asChild?: boolean }) {
  const { onOpenChange } = useDialogContext();
  const handleClick = useCallback(() => onOpenChange(true), [onOpenChange]);

  if (asChild) {
    return <span onClick={handleClick}>{children}</span>;
  }
  return <button onClick={handleClick}>{children}</button>;
}

export function DialogContent({ children, className }: { children: ReactNode; className?: string }) {
  const { open, onOpenChange } = useDialogContext();
  const overlayRef = useRef<HTMLDivElement>(null);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (e.key === 'Escape') onOpenChange(false);
    },
    [onOpenChange],
  );

  useEffect(() => {
    if (open) {
      document.addEventListener('keydown', handleKeyDown);
      return () => document.removeEventListener('keydown', handleKeyDown);
    }
  }, [open, handleKeyDown]);

  const handleOverlayClick = useCallback(
    (e: React.MouseEvent) => {
      if (e.target === overlayRef.current) onOpenChange(false);
    },
    [onOpenChange],
  );

  if (!open) return null;

  return (
    <div ref={overlayRef} className='fixed inset-0 z-50 flex items-center justify-center bg-black/80' onClick={handleOverlayClick}>
      <div className={cn('relative w-full max-w-lg rounded-lg border bg-background p-6 shadow-lg', 'animate-in fade-in-0 zoom-in-95', className)}>
        <button className='absolute right-4 top-4 rounded-sm opacity-70 hover:opacity-100' onClick={() => onOpenChange(false)}>
          ✕
        </button>
        {children}
      </div>
    </div>
  );
}

export function DialogHeader({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn('flex flex-col space-y-1.5 text-center sm:text-left', className)}>{children}</div>;
}

export function DialogTitle({ children, className }: { children: ReactNode; className?: string }) {
  return <h2 className={cn('text-lg font-semibold leading-none tracking-tight', className)}>{children}</h2>;
}

export function DialogFooter({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn('flex flex-col-reverse sm:flex-row sm:justify-end sm:space-x-2', className)}>{children}</div>;
}
