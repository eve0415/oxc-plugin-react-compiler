import { useMemo } from 'react'
import { Badge } from './ui/Badge'
import { Avatar } from './ui/Avatar'

export interface Activity {
  id: number
  user: string
  action: string
  target: string
  timestamp: Date
}

export function ActivityItem({ activity }: { activity: Activity }) {
  const timeAgo = useMemo(() => {
    const diff = Date.now() - activity.timestamp.getTime()
    const hours = Math.floor(diff / 3600000)
    if (hours < 1) return 'Just now'
    if (hours < 24) return `${hours}h ago`
    return `${Math.floor(hours / 24)}d ago`
  }, [activity.timestamp])

  const actionBadgeVariant = useMemo(() => {
    switch (activity.action) {
      case 'created': return 'default' as const
      case 'updated': return 'secondary' as const
      case 'deleted': return 'destructive' as const
      default: return 'outline' as const
    }
  }, [activity.action])

  return (
    <div className="flex items-center space-x-3 py-2">
      <Avatar fallback={activity.user[0]!} size="sm" />
      <div className="flex-1 min-w-0">
        <p className="text-sm">
          <span className="font-medium">{activity.user}</span>{' '}
          <Badge variant={actionBadgeVariant} className="mx-1">{activity.action}</Badge>{' '}
          {activity.target}
        </p>
      </div>
      <span className="text-xs text-muted-foreground whitespace-nowrap">{timeAgo}</span>
    </div>
  )
}
