import { useState, useMemo } from 'react';

import { Avatar } from './ui/Avatar';
import { Badge } from './ui/Badge';
import { Card, CardHeader, CardTitle, CardContent } from './ui/Card';
import { SkeletonCard } from './ui/Skeleton';
import { Tooltip } from './ui/Tooltip';

interface Stat {
  label: string;
  value: number;
  change: number;
  trend: 'up' | 'down' | 'neutral';
}

interface Activity {
  id: number;
  user: string;
  action: string;
  target: string;
  timestamp: Date;
}

const stats: Stat[] = [
  { label: 'Total Users', value: 2847, change: 12.5, trend: 'up' },
  { label: 'Active Sessions', value: 342, change: -3.2, trend: 'down' },
  { label: 'Revenue', value: 45230, change: 8.1, trend: 'up' },
  { label: 'Conversion Rate', value: 3.2, change: 0.0, trend: 'neutral' },
];

const activities: Activity[] = Array.from({ length: 20 }, (_, i) => ({
  id: i,
  user: `User ${i + 1}`,
  action: ['created', 'updated', 'deleted', 'viewed'][i % 4]!,
  target: ['Project Alpha', 'Document Beta', 'Task Gamma', 'Report Delta'][i % 4]!,
  timestamp: new Date(Date.now() - i * 3600000),
}));

function StatCard({ stat }: { stat: Stat }) {
  const trendColor = stat.trend === 'up' ? 'text-green-500' : stat.trend === 'down' ? 'text-red-500' : 'text-gray-500';
  const trendIcon = stat.trend === 'up' ? '↑' : stat.trend === 'down' ? '↓' : '→';

  const formattedValue = useMemo(() => {
    if (stat.value >= 1000) return `${(stat.value / 1000).toFixed(1)}k`;
    return stat.value.toString();
  }, [stat.value]);

  return (
    <Card>
      <CardHeader className='pb-2'>
        <CardTitle className='text-sm font-medium text-muted-foreground'>{stat.label}</CardTitle>
      </CardHeader>
      <CardContent>
        <div className='text-2xl font-bold'>{formattedValue}</div>
        <Tooltip content={`${stat.change > 0 ? '+' : ''}${stat.change}% from last period`}>
          <span className={`text-xs ${trendColor}`}>
            {trendIcon} {Math.abs(stat.change)}%
          </span>
        </Tooltip>
      </CardContent>
    </Card>
  );
}

function ActivityItem({ activity }: { activity: Activity }) {
  const timeAgo = useMemo(() => {
    const diff = Date.now() - activity.timestamp.getTime();
    const hours = Math.floor(diff / 3600000);
    if (hours < 1) return 'Just now';
    if (hours < 24) return `${hours}h ago`;
    return `${Math.floor(hours / 24)}d ago`;
  }, [activity.timestamp]);

  const actionBadgeVariant = useMemo(() => {
    switch (activity.action) {
      case 'created':
        return 'default' as const;
      case 'updated':
        return 'secondary' as const;
      case 'deleted':
        return 'destructive' as const;
      default:
        return 'outline' as const;
    }
  }, [activity.action]);

  return (
    <div className='flex items-center space-x-3 py-2'>
      <Avatar fallback={activity.user[0]!} size='sm' />
      <div className='flex-1 min-w-0'>
        <p className='text-sm'>
          <span className='font-medium'>{activity.user}</span>{' '}
          <Badge variant={actionBadgeVariant} className='mx-1'>
            {activity.action}
          </Badge>{' '}
          {activity.target}
        </p>
      </div>
      <span className='text-xs text-muted-foreground whitespace-nowrap'>{timeAgo}</span>
    </div>
  );
}

export function Dashboard() {
  const [loading] = useState(false);

  const recentActivities = useMemo(() => activities.slice(0, 10), []);

  if (loading) {
    return (
      <div className='space-y-6'>
        <div className='grid grid-cols-4 gap-4'>
          {Array.from({ length: 4 }, (_, i) => (
            <SkeletonCard key={i} />
          ))}
        </div>
      </div>
    );
  }

  return (
    <div className='space-y-6'>
      <div>
        <h1 className='text-3xl font-bold'>Dashboard</h1>
        <p className='text-muted-foreground'>Overview of your application metrics.</p>
      </div>

      <div className='grid grid-cols-4 gap-4'>
        {stats.map(stat => (
          <StatCard key={stat.label} stat={stat} />
        ))}
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Recent Activity</CardTitle>
        </CardHeader>
        <CardContent>
          <div className='divide-y'>
            {recentActivities.map(activity => (
              <ActivityItem key={activity.id} activity={activity} />
            ))}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
