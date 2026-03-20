import { useState, useMemo } from 'react'
import { Card, CardHeader, CardTitle, CardContent } from './ui/Card'
import { SkeletonCard } from './ui/Skeleton'
import { StatCard } from './StatCard'
import type { Stat } from './StatCard'
import { ActivityItem } from './ActivityItem'
import type { Activity } from './ActivityItem'

const stats: Stat[] = [
  { label: 'Total Users', value: 2847, change: 12.5, trend: 'up' },
  { label: 'Active Sessions', value: 342, change: -3.2, trend: 'down' },
  { label: 'Revenue', value: 45230, change: 8.1, trend: 'up' },
  { label: 'Conversion Rate', value: 3.2, change: 0.0, trend: 'neutral' },
]

const activities: Activity[] = Array.from({ length: 20 }, (_, i) => ({
  id: i,
  user: `User ${i + 1}`,
  action: ['created', 'updated', 'deleted', 'viewed'][i % 4]!,
  target: ['Project Alpha', 'Document Beta', 'Task Gamma', 'Report Delta'][i % 4]!,
  timestamp: new Date(Date.now() - i * 3600000),
}))

export function Dashboard() {
  const [loading] = useState(false)
  const recentActivities = useMemo(() => activities.slice(0, 10), [])

  if (loading) {
    return (
      <div className="space-y-6">
        <div className="grid grid-cols-4 gap-4">
          {Array.from({ length: 4 }, (_, i) => (
            <SkeletonCard key={i} />
          ))}
        </div>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-3xl font-bold">Dashboard</h1>
        <p className="text-muted-foreground">Overview of your application metrics.</p>
      </div>
      <div className="grid grid-cols-4 gap-4">
        {stats.map((stat) => (
          <StatCard key={stat.label} stat={stat} />
        ))}
      </div>
      <Card>
        <CardHeader>
          <CardTitle>Recent Activity</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="divide-y">
            {recentActivities.map((activity) => (
              <ActivityItem key={activity.id} activity={activity} />
            ))}
          </div>
        </CardContent>
      </Card>
    </div>
  )
}
