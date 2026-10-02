import { useEffect, useReducer } from 'react'
import { Button, Card, Empty, Flex, List, message, Progress, Select, Space, Spin, Tag, Typography } from 'antd'
import { RobotOutlined } from '@ant-design/icons'
import { api, fmtTime, type Trend } from './api'
import { JobRunningCard } from './jobs'
import { Md } from './md'

const { Text, Link } = Typography

type AiText = { text: string; model: string; truncated: boolean }

/**
 * Everything this tab has fetched or asked the AI for, for ONE time window.
 *
 * It lives outside the component on purpose. App.tsx renders only the open tab
 * (`active === key ? node : null`), so switching tabs UNMOUNTS this one — with
 * state inside the component, a summary still in flight resolved into a dead
 * component and its answer was thrown away, and finished summaries vanished
 * too. Both are exactly what the running card promises will not happen
 * ("sang tab khác, kết quả vẫn được lưu lại").
 *
 * Timestamps rather than tick counters: an elapsed clock computed from
 * `Date.now()` stays right across an unmount, where a counter restarts at 0.
 */
type WindowState = {
  trends: Trend[] | null
  articleCount: number
  analysis: AiText | null
  analysisStartedAt: number
  /** phrase → the summary the AI wrote for it. */
  summaries: Record<string, AiText>
  /** phrase → when its summary was requested. Present only while running. */
  startedAt: Record<string, number>
}

/**
 * One bucket per window: a phrase from the 24h list is not the same trend as
 * the same phrase over 96h, so their summaries must not overwrite each other.
 */
const windows = new Map<number, WindowState>()

function windowState(hours: number): WindowState {
  let w = windows.get(hours)
  if (!w) {
    w = {
      trends: null,
      articleCount: 0,
      analysis: null,
      analysisStartedAt: 0,
      summaries: {},
      startedAt: {},
    }
    windows.set(hours, w)
  }
  return w
}

/** The window last chosen, so reopening the tab lands where it was left. */
let lastHours = 48

const secondsSince = (at: number) => Math.round((Date.now() - at) / 1000)

export default function TrendsTab() {
  const [, render] = useReducer((n: number) => n + 1, 0)
  const hours = lastHours
  const w = windowState(hours)

  const setHours = (h: number) => {
    lastHours = h
    render()
  }

  useEffect(() => {
    // Refetch on every mount so a tab left open for an hour is not stale, but
    // keep showing what is already there — a spinner over a list the user was
    // just reading is the flicker this store exists to avoid.
    let alive = true
    api.trends(hours).then((r) => {
      if (!alive) return
      const cur = windowState(hours)
      cur.trends = r.trends
      cur.articleCount = r.article_count
      render()
    })
    return () => {
      alive = false
    }
  }, [hours])

  const running = Object.keys(w.startedAt).length + (w.analysisStartedAt ? 1 : 0)

  useEffect(() => {
    // Ticks the elapsed clocks, and paints the answer of a run that finished
    // while this tab was closed.
    if (running === 0) return
    const t = window.setInterval(render, 500)
    return () => window.clearInterval(t)
  }, [running])

  const analyze = async () => {
    const cur = windowState(hours)
    if (cur.analysisStartedAt) return
    cur.analysisStartedAt = Date.now()
    render()
    try {
      const r = await api.analyzeTrends(hours)
      if (r.error) message.error(String(r.error))
      else cur.analysis = { text: r.analysis, model: r.model, truncated: !!r.truncated }
    } finally {
      cur.analysisStartedAt = 0
      render()
    }
  }

  const summarize = async (phrase: string) => {
    const cur = windowState(hours)
    if (cur.startedAt[phrase]) return
    cur.startedAt[phrase] = Date.now()
    render()
    try {
      const r = await api.trendSummary(phrase, hours)
      if (r.error) message.error(String(r.error))
      else cur.summaries[phrase] = { text: r.summary, model: r.model, truncated: !!r.truncated }
    } finally {
      delete cur.startedAt[phrase]
      render()
    }
  }

  if (!w.trends) return <Spin style={{ marginTop: 40 }} />
  const trends = w.trends
  const maxScore = Math.max(1, ...trends.map((t) => t.score))

  return (
    <Space direction="vertical" size={14} style={{ width: '100%' }}>
      <Flex gap={8} align="center" wrap>
        <Select
          value={hours}
          onChange={setHours}
          style={{ width: 180 }}
          options={[
            { value: 12, label: '12 giờ so với 12 giờ trước' },
            { value: 24, label: '24 giờ so với 24 giờ trước' },
            { value: 48, label: '48 giờ so với 48 giờ trước' },
            { value: 96, label: '4 ngày so với 4 ngày trước' },
          ]}
        />
        <Text type="secondary">{w.articleCount} bài trong cửa sổ hiện tại</Text>
        <Button
          type="primary"
          icon={<RobotOutlined />}
          loading={w.analysisStartedAt > 0}
          onClick={analyze}
          disabled={trends.length === 0}
        >
          AI nhận định xu hướng
        </Button>
      </Flex>

      {w.analysisStartedAt > 0 ? (
        <JobRunningCard
          label={`Đang phân tích xu hướng ${hours}h`}
          elapsed={secondsSince(w.analysisStartedAt)}
        />
      ) : (
        w.analysis && (
          <Card size="small" title={`Nhận định AI${w.analysis.model ? ` · ${w.analysis.model}` : ''}`}>
            <Md text={w.analysis.text} truncated={w.analysis.truncated} />
          </Card>
        )
      )}

      {trends.length === 0 ? (
        <Empty description="Chưa phát hiện cụm từ nào tăng nhiệt — cần thêm bài (bấm Thu thập ngay, đợi vài chu kỳ quét)" />
      ) : (
        <List
          grid={{ gutter: 12, column: 2 }}
          dataSource={trends}
          renderItem={(t) => {
            const startedAt = w.startedAt[t.phrase]
            const summary = w.summaries[t.phrase]
            return (
              <List.Item>
                <Card size="small">
                  <Space direction="vertical" size={6} style={{ width: '100%' }}>
                    <Flex justify="space-between" align="center">
                      <Text strong style={{ fontSize: 15 }}>{t.phrase}</Text>
                      <Space size={4}>
                        <Tag color="geekblue">{t.count} bài</Tag>
                        <Text type="secondary" style={{ fontSize: 12 }}>kỳ trước: {t.prev_count}</Text>
                      </Space>
                    </Flex>
                    <Progress
                      percent={Math.round((t.score / maxScore) * 100)}
                      showInfo={false}
                      strokeColor="#0ea5e9"
                      size="small"
                    />
                    {t.samples.map((s) => (
                      <div key={s.id}>
                        <Link href={s.url} target="_blank" style={{ fontSize: 13 }} ellipsis>
                          {s.title}
                        </Link>
                        <Text type="secondary" style={{ fontSize: 12 }}> — {s.source} · {fmtTime(s.published_at)}</Text>
                      </div>
                    ))}

                    {startedAt ? (
                      <JobRunningCard
                        label={`Đang tóm tắt: ${t.phrase}`}
                        elapsed={secondsSince(startedAt)}
                      />
                    ) : (
                      summary && (
                        <Card
                          size="small"
                          type="inner"
                          title={`Tóm tắt AI${summary.model ? ` · ${summary.model}` : ''}`}
                        >
                          <Md text={summary.text} truncated={summary.truncated} />
                        </Card>
                      )
                    )}
                    <Button
                      type="text"
                      size="small"
                      icon={<RobotOutlined />}
                      loading={!!startedAt}
                      disabled={running > 0 && !startedAt}
                      onClick={() => summarize(t.phrase)}
                      style={{ paddingLeft: 0 }}
                    >
                      {summary ? 'Tóm tắt lại' : 'AI tóm tắt xu hướng này'}
                    </Button>
                  </Space>
                </Card>
              </List.Item>
            )
          }}
        />
      )}
    </Space>
  )
}
