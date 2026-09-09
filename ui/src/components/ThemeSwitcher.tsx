import { useState } from 'react'
import { MonitorIcon, MoonIcon, SunIcon } from 'lucide-react'
import { Button } from '@/components/ui'
import { THEME_MODES, readThemeMode, writeThemeMode, type ThemeMode } from '@/lib/theme'

const ICONS = { system: MonitorIcon, light: SunIcon, dark: MoonIcon } as const
const LABELS: Record<ThemeMode, string> = { system: '跟随系统', light: '浅色', dark: '深色' }

export function ThemeSwitcher() {
  const [mode, setMode] = useState<ThemeMode>(readThemeMode)
  const Icon = ICONS[mode]
  const next = () => {
    const i = THEME_MODES.indexOf(mode)
    const m = THEME_MODES[(i + 1) % THEME_MODES.length]
    writeThemeMode(m)
    setMode(m)
  }
  return (
    <Button variant="ghost" className="px-2.5" onClick={next} title={`主题：${LABELS[mode]}（点击切换）`}>
      <Icon className="size-4" />
    </Button>
  )
}
