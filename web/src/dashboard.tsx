import { useEffect, useState } from 'react'
import { Card, Col, Empty, Flex, List, Row, Space, Spin, Statistic, Tag, Tooltip, Typography } from 'antd'
import { api, fmtTime, type Dashboard } from './api'

const { Text, Link } = Typography

export default function DashboardTab({ onOpenTab }: { onOpenTab: (key: string) => void }) {
  const [d, setD] = useState<Dashboard | null>(null)

  useEffect(() => {
    api.dashboard().then(setD).catch(() => {})
  }, [])

  if (!d) return <Spin style={{ marginTop: 40 }} />

  const maxDay = Math.max(1, ...d.per_day.map((p) => p.count))

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Row gutter={16}>
        <Col span={6}>
          <Card size="small"><Statistic title="Tổng số bài" value={d.articles_total} /></Card>
        </Col>
        <Col span={6}>
          <Card size="small"><Statistic title="Bài trong 24h" value={d.articles_24h} /></Card>
        </Col>
        <Col span={6}>
          <Card size="small"><Statistic title="Nguồn hoạt động" value={d.sources_active} /></Card>
        </Col>
        <Col span={6}>
          <Card size="small">
            <Statistic title="Lần quét gần nhất" value={d.last_fetch_at ? fmtTime(d.last_fetch_at) : '—'} />
          </Card>
        </Col>
      </Row>

      <Row gutter={16}>
        <Col span={12}>
          <Card size="small" title="Số bài theo ngày (14 ngày)">
            <Flex align="flex-end" gap={4} style={{ height: 120 }}>
              {d.per_day.map((p) => (
                <Tooltip key={p.day} title={`${p.day}: ${p.count} bài`}>
                  <div
                    style={{
                      flex: 1,
                      height: Math.max(3, (p.count / maxDay) * 110),
                      background: '#0ea5e9',
                      opacity: 0.35 + 0.55 * (p.count / maxDay),
                      borderRadius: 3,
                    }}
                  />
                </Tooltip>
              ))}
            </Flex>
          </Card>
        </Col>
        <Col span={12}>
          <Card
            size="small"
            title="Cụm từ tăng nhiệt (48h)"
            extra={<a onClick={() => onOpenTab('trends')}>Xem xu hướng →</a>}
          >
            {d.trends.length === 0 ? (
              <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="Chưa đủ dữ liệu — bấm Thu thập ngay" />
            ) : (
              <Space size={[6, 8]} wrap>
                {d.trends.slice(0, 12).map((t) => (
                  <Tag key={t.phrase} color="geekblue" style={{ fontSize: 13 }}>
                    {t.phrase} <Text type="secondary" style={{ fontSize: 11 }}>×{t.count}</Text>
                  </Tag>
                ))}
              </Space>
            )}
          </Card>
        </Col>
      </Row>

      <Row gutter={16}>
        <Col span={10}>
          <Card
            size="small"
            title="Dòng sự kiện nóng"
            extra={<a onClick={() => onOpenTab('stories')}>Tất cả →</a>}
          >
            <List
              size="small"
              dataSource={d.hot_stories}
              locale={{ emptyText: 'Chưa có sự kiện nào được nhiều nguồn cùng đưa' }}
              renderItem={(s) => (
                <List.Item>
                  <Space direction="vertical" size={0} style={{ width: '100%' }}>
                    <Text strong ellipsis>{s.title}</Text>
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      {s.article_count} bài · cập nhật {fmtTime(s.last_at)}
                    </Text>
                  </Space>
                </List.Item>
              )}
            />
          </Card>
        </Col>
        <Col span={14}>
          <Card
            size="small"
            title="Bài mới nhất"
            extra={<a onClick={() => onOpenTab('articles')}>Tất cả →</a>}
          >
            <List
              size="small"
              dataSource={d.recent_articles}
              locale={{ emptyText: 'Chưa có bài — bấm Thu thập ngay' }}
              renderItem={(a) => (
                <List.Item>
                  <Space direction="vertical" size={0} style={{ width: '100%' }}>
                    <Link href={a.url} target="_blank" ellipsis>{a.title}</Link>
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      {a.source_name} · {fmtTime(a.published_at)}
                    </Text>
                  </Space>
                </List.Item>
              )}
            />
          </Card>
        </Col>
      </Row>

      {d.top_topics.length > 0 && (
        <Card size="small" title="Chủ đề nhiều bài (7 ngày)">
          <Space size={[8, 8]} wrap>
            {d.top_topics.map((t) => (
              <Tag key={t.id} color={t.color || 'blue'}>
                {t.name}: {t.count} bài
              </Tag>
            ))}
          </Space>
        </Card>
      )}
    </Space>
  )
}
