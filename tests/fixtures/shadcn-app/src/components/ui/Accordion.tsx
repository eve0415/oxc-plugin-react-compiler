import { createContext, useContext, useState, useCallback, type ReactNode } from 'react'
import { cn } from '../../lib/utils'

interface AccordionContextValue {
  openItems: Set<string>
  toggle: (value: string) => void
}

const AccordionContext = createContext<AccordionContextValue | null>(null)

function useAccordionContext(): AccordionContextValue {
  const ctx = useContext(AccordionContext)
  if (!ctx) throw new Error('Accordion components must be used within an Accordion')
  return ctx
}

export function Accordion({
  type = 'single',
  children,
  className,
}: {
  type?: 'single' | 'multiple'
  children: ReactNode
  className?: string
}) {
  const [openItems, setOpenItems] = useState<Set<string>>(new Set())

  const toggle = useCallback(
    (value: string) => {
      setOpenItems((prev) => {
        const next = new Set(prev)
        if (next.has(value)) {
          next.delete(value)
        } else {
          if (type === 'single') next.clear()
          next.add(value)
        }
        return next
      })
    },
    [type],
  )

  return (
    <AccordionContext.Provider value={{ openItems, toggle }}>
      <div className={className}>{children}</div>
    </AccordionContext.Provider>
  )
}

export function AccordionItem({
  value,
  children,
  className,
}: {
  value: string
  children: ReactNode
  className?: string
}) {
  return <div className={cn('border-b', className)}>{children}</div>
}

export function AccordionTrigger({
  value,
  children,
  className,
}: {
  value: string
  children: ReactNode
  className?: string
}) {
  const { openItems, toggle } = useAccordionContext()
  const isOpen = openItems.has(value)
  const handleClick = useCallback(() => toggle(value), [value, toggle])

  return (
    <h3 className="flex">
      <button
        className={cn(
          'flex flex-1 items-center justify-between py-4 font-medium transition-all hover:underline',
          className,
        )}
        onClick={handleClick}
      >
        {children}
        <span className={cn('h-4 w-4 shrink-0 transition-transform', isOpen && 'rotate-180')}>
          ▾
        </span>
      </button>
    </h3>
  )
}

export function AccordionContent({
  value,
  children,
  className,
}: {
  value: string
  children: ReactNode
  className?: string
}) {
  const { openItems } = useAccordionContext()
  if (!openItems.has(value)) return null

  return (
    <div className={cn('overflow-hidden text-sm transition-all', className)}>
      <div className="pb-4 pt-0">{children}</div>
    </div>
  )
}
