import { useMemo } from 'react'
import { Card, CardHeader, CardTitle, CardContent } from './ui/Card'
import { Tooltip } from './ui/Tooltip'

interface Stat {
  label: string
  value: number
  change: number
  trend: 'up' | 'down' | 'neutral'
}

export type { Stat }

export function StatCard({ stat }: { stat: Stat }) {
  const trendColor =
    stat.trend === 'up' ? 'text-green-500' : stat.trend === 'down' ? 'text-red-500' : 'text-gray-500'
  const trendIcon = stat.trend === 'up' ? '↑' : stat.trend === 'down' ? '↓' : '→'

  const formattedValue = useMemo(
    () => stat.value >= 1000 ? `${(stat.value / 1000).toFixed(1)}k` : stat.value.toString(),
    [stat.value],
  )

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium text-muted-foreground">{stat.label}</CardTitle>
      </CardHeader>
      <CardContent>
        <div className="text-2xl font-bold">{formattedValue}</div>
        <Tooltip content={`${stat.change > 0 ? '+' : ''}${stat.change}% from last period`}>
          <span className={`text-xs ${trendColor}`}>
            {trendIcon} {Math.abs(stat.change)}%
          </span>
        </Tooltip>
      </CardContent>
    </Card>
  )
}
