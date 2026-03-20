import { useState, useCallback, useRef, type ReactNode } from 'react'
import { cn } from '../../lib/utils'

interface TooltipProps {
  content: ReactNode
  children: ReactNode
  side?: 'top' | 'bottom' | 'left' | 'right'
  delayMs?: number
}

export function Tooltip({ content, children, side = 'top', delayMs = 200 }: TooltipProps) {
  const [visible, setVisible] = useState(false)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  const show = useCallback(() => {
    timerRef.current = setTimeout(() => setVisible(true), delayMs)
  }, [delayMs])

  const hide = useCallback(() => {
    if (timerRef.current) clearTimeout(timerRef.current)
    setVisible(false)
  }, [])

  const positionClasses: Record<string, string> = {
    top: 'bottom-full left-1/2 -translate-x-1/2 mb-2',
    bottom: 'top-full left-1/2 -translate-x-1/2 mt-2',
    left: 'right-full top-1/2 -translate-y-1/2 mr-2',
    right: 'left-full top-1/2 -translate-y-1/2 ml-2',
  }

  return (
    <div className="relative inline-block" onMouseEnter={show} onMouseLeave={hide}>
      {children}
      {visible && (
        <div
          className={cn(
            'absolute z-50 rounded-md border bg-popover px-3 py-1.5 text-sm shadow-md',
            'animate-in fade-in-0 zoom-in-95',
            positionClasses[side],
          )}
          role="tooltip"
        >
          {content}
        </div>
      )}
    </div>
  )
}
