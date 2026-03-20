import { useState, useCallback, type ImgHTMLAttributes } from 'react'
import { cn } from '../../lib/utils'

interface AvatarProps {
  src?: string
  alt?: string
  fallback: string
  className?: string
  size?: 'sm' | 'md' | 'lg'
}

const sizeClasses: Record<string, string> = {
  sm: 'h-8 w-8 text-xs',
  md: 'h-10 w-10 text-sm',
  lg: 'h-12 w-12 text-base',
}

export function Avatar({ src, alt, fallback, className, size = 'md' }: AvatarProps) {
  const [imageError, setImageError] = useState(false)
  const handleError = useCallback(() => setImageError(true), [])

  const showFallback = !src || imageError

  return (
    <div
      className={cn(
        'relative flex shrink-0 overflow-hidden rounded-full',
        sizeClasses[size],
        className,
      )}
    >
      {showFallback ? (
        <div className="flex h-full w-full items-center justify-center rounded-full bg-muted">
          {fallback}
        </div>
      ) : (
        <img
          className="aspect-square h-full w-full object-cover"
          src={src}
          alt={alt}
          onError={handleError}
        />
      )}
    </div>
  )
}
