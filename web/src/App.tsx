import { useEffect, useState } from 'react'
import { Badge, Button, Flex, message, Segmented, Space, Tabs, Tag, Tooltip, Typography } from 'antd'
import { CloudDownloadOutlined, DesktopOutlined, MoonOutlined, SunOutlined } from '@ant-design/icons'
import { api, type Status } from './api'
import { JobBadge, useJobs } from './jobs'
import { useTheme, type ThemeMode } from './theme'
import DashboardTab from './dashboard'
import ArticlesTab from './articles'
import TrendsTab from './trends'
import StoriesTab from './stories'
import DigestTab from './digest'
import { SourcesTab, TopicsTab } from './panels'

const { Title, Text } = Typography

export default function App() {
  const [status, setStatus] = useState<Status | null>(null)
  const [active, setActive] = useState('dashboard')
  const [fetching, setFetching] = useState(false)
  const { mode, setMode } = useTheme()
  const jobs = useJobs()

  const refreshStatus = () => api.status().then(setStatus).catch(() => {})
  useEffect(() => {
    refreshStatus()
    const t = setInterval(refreshStatus, 20000)
    return () => clearInterval(t)
  }, [])

  const fetchNow = async () => {
    setFetching(true)
    try {
      const r = await api.fetchAll()
      if (r.error) message.error(String(r.error))
      else message.success(`Quét ${r.sources} nguồn: ${r.new} bài mới${r.errors?.length ? `, ${r.errors.length} nguồn lỗi` : ''}`)
      refreshStatus()
    } finally {
      setFetching(false)
    }
  }

  // Render only the active tab so switching always refetches fresh data.
  const tab = (key: string, node: React.ReactNode) => (active === key ? node : null)

  return (
    <div style={{ maxWidth: 1240, margin: '0 auto', padding: 24 }}>
      <Flex align="center" justify="space-between" style={{ marginBottom: 8 }}>
        <Title level={3} style={{ margin: 0 }}>
          📰 Tin Tức <Text type="secondary" style={{ fontSize: 14 }}>— SenClaw</Text>
        </Title>
        <Space>
          <JobBadge jobs={jobs} />
          <Tag color="blue">{status?.articles_24h ?? 0} bài / 24h</Tag>
          {status && status.sources_error > 0 ? (
            <Badge status="error" text={`${status.sources_error} nguồn lỗi`} />
          ) : (
            <Badge status="success" text={`${status?.sources_active ?? 0} nguồn hoạt động`} />
          )}
          <Button icon={<CloudDownloadOutlined />} loading={fetching} onClick={fetchNow} type="primary" size="small">
            Thu thập ngay
          </Button>
          <Tooltip title="Giao diện sáng / tối / theo hệ thống">
            <Segmented
              size="small"
              value={mode}
              onChange={(v) => setMode(v as ThemeMode)}
              options={[
                { value: 'light', icon: <SunOutlined /> },
                { value: 'dark', icon: <MoonOutlined /> },
                { value: 'system', icon: <DesktopOutlined /> },
              ]}
            />
          </Tooltip>
        </Space>
      </Flex>

      <Tabs
        activeKey={active}
        onChange={setActive}
        items={[
          { key: 'dashboard', label: 'Tổng quan', children: tab('dashboard', <DashboardTab onOpenTab={setActive} />) },
          { key: 'articles', label: 'Tin tức', children: tab('articles', <ArticlesTab />) },
          { key: 'trends', label: 'Xu hướng', children: tab('trends', <TrendsTab />) },
          { key: 'stories', label: 'Dòng sự kiện', children: tab('stories', <StoriesTab />) },
          { key: 'digest', label: 'Điểm tin AI', children: tab('digest', <DigestTab />) },
          { key: 'topics', label: 'Chủ đề', children: tab('topics', <TopicsTab />) },
          {
            key: 'sources',
            label: (
              <Badge count={status?.sources_error ?? 0} size="small" offset={[8, -2]}>
                Nguồn tin
              </Badge>
            ),
            children: tab('sources', <SourcesTab onChange={refreshStatus} />),
          },
        ]}
      />
    </div>
  )
}
