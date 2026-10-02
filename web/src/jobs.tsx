// Trạng thái "đang xử lý" dùng chung. Job sống ở SERVER, nên đổi tab, mở cửa
// sổ thứ hai hay F5 giữa chừng vẫn thấy đúng việc đang chạy — kể cả việc do
// agent gọi qua MCP.

import { useEffect, useRef, useState } from 'react'
import { Card, Flex, Space, Spin, Typography } from 'antd'
import { api, type Job } from './api'

const { Text } = Typography

/** Poll /api/jobs. Nhanh khi đang có việc, thưa khi rảnh. */
export function useJobs(): Job[] {
  const [jobs, setJobs] = useState<Job[]>([])
  const busy = useRef(false)

  useEffect(() => {
    let alive = true
    let timer: number | undefined

    const tick = async () => {
      try {
        const r = await api.jobs()
        if (!alive) return
        setJobs(r.jobs)
        busy.current = r.jobs.length > 0
      } catch {
        /* server tạm không trả lời — lần sau thử lại */
      }
      if (alive) timer = window.setTimeout(tick, busy.current ? 2000 : 6000)
    }

    tick()
    return () => {
      alive = false
      if (timer) window.clearTimeout(timer)
    }
  }, [])

  return jobs
}

export const fmtElapsed = (sec: number) => {
  const s = Math.max(0, Math.round(sec))
  return s < 60 ? `${s} giây` : `${Math.floor(s / 60)} phút ${s % 60} giây`
}

/** Khối "đang xử lý" đầy đủ: đặt vào chỗ kết quả sẽ hiện ra. */
export function JobRunningCard({
  label,
  elapsed,
  hint,
}: {
  label: string
  elapsed: number
  hint?: string
}) {
  return (
    <Card size="small">
      <Flex gap={14} align="flex-start">
        <Spin />
        <Space direction="vertical" size={2} style={{ flex: 1 }}>
          <Text strong>{label}…</Text>
          <Text type="secondary" style={{ fontSize: 13 }}>
            Đã chạy {fmtElapsed(elapsed)}
            {' · '}
            {hint ?? 'AI thường mất 20–60 giây. Bạn có thể sang tab khác, kết quả vẫn được lưu lại.'}
          </Text>
        </Space>
      </Flex>
    </Card>
  )
}

/** Dòng gọn cho header: hiện việc đang chạy sớm nhất. */
export function JobBadge({ jobs }: { jobs: Job[] }) {
  if (!jobs.length) return null
  const j = jobs[0]
  const more = jobs.length - 1
  return (
    <Space size={6}>
      <Spin size="small" />
      <Text type="secondary" style={{ fontSize: 13 }}>
        {j.label} ({fmtElapsed(j.elapsed_sec)})
        {more > 0 ? ` +${more} việc khác` : ''}
      </Text>
    </Space>
  )
}
