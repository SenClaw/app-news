// Dark / light / theo hệ thống. Lựa chọn lưu ở localStorage; chế độ "hệ thống"
// bám theo `prefers-color-scheme` và đổi ngay khi OS đổi (không cần reload).

import { createContext, useContext, useEffect, useMemo, useState } from 'react'
import { ConfigProvider, theme as antdTheme } from 'antd'

export type ThemeMode = 'light' | 'dark' | 'system'

const KEY = 'senclaw-news-theme'

interface Ctx {
  mode: ThemeMode
  setMode: (m: ThemeMode) => void
  /** Chế độ đang hiển thị thật sự (đã giải quyết 'system'). */
  dark: boolean
}

const ThemeCtx = createContext<Ctx>({ mode: 'system', setMode: () => {}, dark: true })

export const useTheme = () => useContext(ThemeCtx)

const prefersDark = () =>
  typeof window !== 'undefined' && window.matchMedia?.('(prefers-color-scheme: dark)').matches

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [mode, setModeState] = useState<ThemeMode>(() => {
    const saved = localStorage.getItem(KEY)
    return saved === 'light' || saved === 'dark' || saved === 'system' ? saved : 'system'
  })
  const [systemDark, setSystemDark] = useState(prefersDark)

  // Only meaningful in 'system' mode, but keeping the listener always on means
  // switching back to 'system' is instantly correct.
  useEffect(() => {
    const mq = window.matchMedia?.('(prefers-color-scheme: dark)')
    if (!mq) return
    const onChange = (e: MediaQueryListEvent) => setSystemDark(e.matches)
    mq.addEventListener('change', onChange)
    return () => mq.removeEventListener('change', onChange)
  }, [])

  const dark = mode === 'system' ? systemDark : mode === 'dark'

  const setMode = (m: ThemeMode) => {
    localStorage.setItem(KEY, m)
    setModeState(m)
  }

  // Drive plain CSS (body background, scrollbars, our own SVG/markdown styles).
  useEffect(() => {
    document.documentElement.dataset.theme = dark ? 'dark' : 'light'
    document.documentElement.style.colorScheme = dark ? 'dark' : 'light'
  }, [dark])

  const value = useMemo(() => ({ mode, setMode, dark }), [mode, dark])

  return (
    <ThemeCtx.Provider value={value}>
      <ConfigProvider
        theme={{
          algorithm: dark ? antdTheme.darkAlgorithm : antdTheme.defaultAlgorithm,
          token: { colorPrimary: '#0ea5e9', borderRadius: 10 },
        }}
      >
        {children}
      </ConfigProvider>
    </ThemeCtx.Provider>
  )
}
