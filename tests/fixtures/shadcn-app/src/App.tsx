import { useState, useCallback } from 'react'
import { ToastProvider } from './components/ui/Toast'
import { Dashboard } from './components/Dashboard'
import { DataTable } from './components/DataTable'
import { UserProfileForm } from './components/UserProfileForm'
import { SettingsPage } from './components/SettingsPage'
import { Button } from './components/ui/Button'

type Page = 'dashboard' | 'users' | 'profile' | 'settings'

function NavButton({ page, current, onClick, label }: { page: Page; current: Page; onClick: (p: Page) => void; label: string }) {
  return (
    <Button
      variant={page === current ? 'default' : 'ghost'}
      onClick={() => onClick(page)}
    >
      {label}
    </Button>
  )
}

function AppContent() {
  const [page, setPage] = useState<Page>('dashboard')

  const navigate = useCallback((p: Page) => setPage(p), [])

  return (
    <div className="min-h-screen bg-background">
      <nav className="border-b">
        <div className="max-w-7xl mx-auto px-4 py-3 flex space-x-2">
          <NavButton page="dashboard" current={page} onClick={navigate} label="Dashboard" />
          <NavButton page="users" current={page} onClick={navigate} label="Users" />
          <NavButton page="profile" current={page} onClick={navigate} label="Profile" />
          <NavButton page="settings" current={page} onClick={navigate} label="Settings" />
        </div>
      </nav>
      <main className="max-w-7xl mx-auto px-4 py-8">
        {page === 'dashboard' && <Dashboard />}
        {page === 'users' && <DataTable />}
        {page === 'profile' && <UserProfileForm />}
        {page === 'settings' && <SettingsPage />}
      </main>
    </div>
  )
}

export function App() {
  return (
    <ToastProvider>
      <AppContent />
    </ToastProvider>
  )
}
