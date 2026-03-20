import { useState, useCallback, useMemo } from 'react'
import { Table, TableHeader, TableBody, TableRow, TableHead, TableCell } from './ui/Table'
import { Input } from './ui/Input'
import { Button } from './ui/Button'
import { Badge } from './ui/Badge'
import { Avatar } from './ui/Avatar'
import { useDebounce } from '../hooks/useDebounce'

interface User {
  id: number
  name: string
  email: string
  role: string
  status: 'active' | 'inactive' | 'pending'
  createdAt: Date
}

const mockUsers: User[] = Array.from({ length: 50 }, (_, i) => ({
  id: i + 1,
  name: `User ${i + 1}`,
  email: `user${i + 1}@example.com`,
  role: ['admin', 'editor', 'viewer'][i % 3]!,
  status: (['active', 'inactive', 'pending'] as const)[i % 3]!,
  createdAt: new Date(2024, 0, i + 1),
}))

type SortKey = 'name' | 'email' | 'role' | 'status' | 'createdAt'
type SortDir = 'asc' | 'desc'

export function DataTable() {
  const [search, setSearch] = useState('')
  const [sortKey, setSortKey] = useState<SortKey>('name')
  const [sortDir, setSortDir] = useState<SortDir>('asc')
  const [page, setPage] = useState(0)
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  const pageSize = 10
  const debouncedSearch = useDebounce(search, 300)

  const handleSort = useCallback(
    (key: SortKey) => {
      if (key === sortKey) {
        setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'))
      } else {
        setSortKey(key)
        setSortDir('asc')
      }
    },
    [sortKey],
  )

  const toggleSelect = useCallback((id: number) => {
    setSelectedIds((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }, [])

  const toggleSelectAll = useCallback(
    (users: User[]) => {
      setSelectedIds((prev) => {
        const allSelected = users.every((u) => prev.has(u.id))
        if (allSelected) return new Set()
        return new Set(users.map((u) => u.id))
      })
    },
    [],
  )

  const filteredUsers = useMemo(() => {
    const q = debouncedSearch.toLowerCase()
    return mockUsers.filter(
      (u) =>
        u.name.toLowerCase().includes(q) ||
        u.email.toLowerCase().includes(q) ||
        u.role.toLowerCase().includes(q),
    )
  }, [debouncedSearch])

  const sortedUsers = useMemo(() => {
    const sorted = [...filteredUsers]
    sorted.sort((a, b) => {
      const aVal = a[sortKey]
      const bVal = b[sortKey]
      const cmp = aVal < bVal ? -1 : aVal > bVal ? 1 : 0
      return sortDir === 'asc' ? cmp : -cmp
    })
    return sorted
  }, [filteredUsers, sortKey, sortDir])

  const pagedUsers = useMemo(
    () => sortedUsers.slice(page * pageSize, (page + 1) * pageSize),
    [sortedUsers, page],
  )

  const totalPages = Math.ceil(sortedUsers.length / pageSize)

  const statusVariant = useCallback((status: User['status']) => {
    switch (status) {
      case 'active': return 'default' as const
      case 'inactive': return 'secondary' as const
      case 'pending': return 'outline' as const
    }
  }, [])

  const SortHeader = useCallback(
    ({ label, field }: { label: string; field: SortKey }) => (
      <TableHead>
        <button
          className="flex items-center space-x-1 hover:text-foreground"
          onClick={() => handleSort(field)}
        >
          <span>{label}</span>
          {sortKey === field && <span>{sortDir === 'asc' ? '↑' : '↓'}</span>}
        </button>
      </TableHead>
    ),
    [sortKey, sortDir, handleSort],
  )

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <Input
          placeholder="Search users..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="max-w-sm"
        />
        {selectedIds.size > 0 && (
          <Badge variant="secondary">{selectedIds.size} selected</Badge>
        )}
      </div>

      <Table>
        <TableHeader>
          <TableRow>
            <TableHead className="w-12">
              <input
                type="checkbox"
                checked={pagedUsers.length > 0 && pagedUsers.every((u) => selectedIds.has(u.id))}
                onChange={() => toggleSelectAll(pagedUsers)}
              />
            </TableHead>
            <SortHeader label="Name" field="name" />
            <SortHeader label="Email" field="email" />
            <SortHeader label="Role" field="role" />
            <SortHeader label="Status" field="status" />
            <SortHeader label="Created" field="createdAt" />
          </TableRow>
        </TableHeader>
        <TableBody>
          {pagedUsers.map((user) => (
            <TableRow key={user.id}>
              <TableCell>
                <input
                  type="checkbox"
                  checked={selectedIds.has(user.id)}
                  onChange={() => toggleSelect(user.id)}
                />
              </TableCell>
              <TableCell>
                <div className="flex items-center space-x-2">
                  <Avatar fallback={user.name[0]!} size="sm" />
                  <span className="font-medium">{user.name}</span>
                </div>
              </TableCell>
              <TableCell>{user.email}</TableCell>
              <TableCell>
                <Badge variant="outline">{user.role}</Badge>
              </TableCell>
              <TableCell>
                <Badge variant={statusVariant(user.status)}>{user.status}</Badge>
              </TableCell>
              <TableCell>{user.createdAt.toLocaleDateString()}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>

      <div className="flex items-center justify-between">
        <p className="text-sm text-muted-foreground">
          Showing {page * pageSize + 1}–{Math.min((page + 1) * pageSize, sortedUsers.length)} of {sortedUsers.length}
        </p>
        <div className="flex space-x-2">
          <Button variant="outline" size="sm" disabled={page === 0} onClick={() => setPage((p) => p - 1)}>
            Previous
          </Button>
          <Button variant="outline" size="sm" disabled={page >= totalPages - 1} onClick={() => setPage((p) => p + 1)}>
            Next
          </Button>
        </div>
      </div>
    </div>
  )
}
