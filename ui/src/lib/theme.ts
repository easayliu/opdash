/**
 * 深浅色主题：系统 / 浅色 / 深色三态，选择存 localStorage。
 * 'system' 时跟随 prefers-color-scheme，切到显式浅/深后系统变化不再干预。
 */
export const THEME_MODES = ['system', 'light', 'dark'] as const
export type ThemeMode = (typeof THEME_MODES)[number]

const KEY = 'opdash.theme'

function query(): MediaQueryList {
  return window.matchMedia('(prefers-color-scheme: dark)')
}

/** 隐私模式 / 禁用存储时 localStorage 会抛异常，一律降级为「跟随系统、不持久化」。 */
export function readThemeMode(): ThemeMode {
  try {
    const raw = localStorage.getItem(KEY)
    return (THEME_MODES as readonly string[]).includes(raw ?? '') ? (raw as ThemeMode) : 'system'
  } catch {
    return 'system'
  }
}

function resolveDark(mode: ThemeMode): boolean {
  return mode === 'system' ? query().matches : mode === 'dark'
}

export function applyThemeMode(mode: ThemeMode): void {
  document.documentElement.classList.toggle('dark', resolveDark(mode))
}

export function writeThemeMode(mode: ThemeMode): void {
  try {
    localStorage.setItem(KEY, mode)
  } catch {
    // 存不下就算了
  }
  applyThemeMode(mode)
}

/** 渲染前调用：先按存下来的选择上色，避免首帧闪一下另一套配色。 */
export function initTheme(): void {
  applyThemeMode(readThemeMode())
  query().addEventListener('change', () => {
    if (readThemeMode() === 'system') applyThemeMode('system')
  })
}
