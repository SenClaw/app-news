import { useCallback, useEffect, useRef, useState } from 'react'
import {
  Button, Card, Col, Empty, Flex, Input, List, message, Popconfirm, Row, Select, Space, Tag, Typography,
} from 'antd'
import { DeleteOutlined, RobotOutlined } from '@ant-design/icons'
import { api, fmtTime, type DigestRecord, type Job, type Topic } from './api'
import { JobRunningCard } from './jobs'
import { Md } from './md'

const { Text } = Typography

export default function DigestTab() {
  const [hours, setHours] = useState(24)
  const [focus, setFocus] = useState('')
  const [topicId, setTopicId] = useState<number | undefined>()
  const [topics, setTopics] = useState<Topic[]>([])
  const [history, setHistory] = useState<DigestRecord[]>([])
  const [running, setRunning] = useState<Job | null>(null)
  const [current, setCurrent] = useState<DigestRecord | null>(null)
  const [busy, setBusy] = useState(false)
  // Bản mới nhất lúc bấm chạy — dùng để nhận ra bản vừa xong khi quay lại tab.
  const lastSeenId = useRef<number | null>(null)

  useEffect(() => {
    api.topics().then((r) => setTopics(r.topics))
  }, [])

  const open = useCallback(async (id: number) => {
    const r = await api.digestGet(id)
    if (r.digest) setCurrent(r.digest)
  }, [])

  /// Nạp lịch sử. Nếu đang có job chạy thì tự hẹn nạp lại cho tới khi xong,
  /// nên rời tab giữa chừng rồi quay lại vẫn thấy kết quả.
  const loadHistory = useCallback(
    async (opts?: { openNewest?: boolean }) => {
      try {
        const r = await api.digestHistory(30)
        setHistory(r.digests)
        setRunning(r.running)
        const newest = r.digests[0]
        const isNew = newest && lastSeenId.current !== null && newest.id !== lastSeenId.current
        if (newest && (opts?.openNewest || isNew)) {
          lastSeenId.current = newest.id
          open(newest.id)
        } else if (newest && lastSeenId.current === null) {
          lastSeenId.current = newest.id
        }
        return r
      } catch {
        return null
      }
    },
    [open],
  )

  // Mở tab: nạp lịch sử, mở bản mới nhất, và bám theo job đang chạy (nếu có).
  useEffect(() => {
    let alive = true
    let timer: number | undefined
    const poll = async () => {
      const r = await loadHistory({ openNewest: !current })
      if (!alive) return
      if (r?.running) timer = window.setTimeout(poll, 2000)
    }
    poll()
    return () => {
      alive = false
      if (timer) window.clearTimeout(timer)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const run = async () => {
    setBusy(true)
    setCurrent(null)
    // Hiện trạng thái ngay, không đợi vòng poll đầu tiên.
    setRunning({ key: 'digest', kind: 'digest', label: `Đang viết điểm tin ${hours}h`, started_at: 0, elapsed_sec: 0 })
    const started = Date.now()
    const ticker = window.setInterval(
      () => setRunning((p) => (p ? { ...p, elapsed_sec: Math.round((Date.now() - started) / 1000) } : p)),
      1000,
    )
    try {
      const r = await api.digest({ hours, focus: focus || undefined, topic_id: topicId })
      if (r.error) {
        message.error(String(r.error))
        setRunning(null)
      } else {
        lastSeenId.current = r.digest_id ?? null
        await loadHistory()
        if (r.digest_id) await open(r.digest_id)
        setRunning(null)
      }
    } catch {
      message.error('Mất kết nối với app — kết quả (nếu đã chạy xong) vẫn nằm trong lịch sử')
      loadHistory()
    } finally {
      window.clearInterval(ticker)
      setBusy(false)
    }
  }

  const remove = async (id: number) => {
    await api.digestDelete(id)
    if (current?.id === id) setCurrent(null)
    loadHistory()
  }

  const scope = (d: DigestRecord) =>
    [`${d.hours}h`, d.topic_name, d.focus].filter(Boolean).join(' · ')

  return (
    <Space direction="vertical" size={14} style={{ width: '100%' }}>
      <Flex gap={8} wrap align="center">
        <Select
          value={hours}
          onChange={setHours}
          style={{ width: 140 }}
          options={[
            { value: 6, label: '6 giờ qua' },
            { value: 12, label: '12 giờ qua' },
            { value: 24, label: '24 giờ qua' },
            { value: 72, label: '3 ngày qua' },
            { value: 168, label: '7 ngày qua' },
          ]}
        />
        <Select
          placeholder="Giới hạn một chủ đề (tuỳ chọn)"
          allowClear
          style={{ width: 230 }}
          value={topicId}
          onChange={setTopicId}
          options={topics.map((t) => ({ value: t.id, label: t.name }))}
        />
        <Input
          placeholder="Trọng tâm quan tâm, ví dụ: công nghệ, kinh tế vĩ mô…"
          style={{ width: 300 }}
          value={focus}
          onChange={(e) => setFocus(e.target.value)}
        />
        <Button type="primary" icon={<RobotOutlined />} loading={busy || !!running} onClick={run} disabled={!!running}>
          {running ? 'Đang viết…' : 'Viết điểm tin'}
        </Button>
      </Flex>

      <Row gutter={16}>
        <Col span={7}>
          <Card size="small" title={`Lịch sử điểm tin${history.length ? ` (${history.length})` : ''}`}>
            <List
              size="small"
              dataSource={history}
              locale={{ emptyText: 'Chưa có bản nào — bấm Viết điểm tin' }}
              renderItem={(d) => (
                <List.Item
                  onClick={() => open(d.id)}
                  style={{
                    cursor: 'pointer',
                    borderRadius: 8,
                    padding: '8px 10px',
                    background: current?.id === d.id ? 'rgba(14,165,233,0.14)' : undefined,
                  }}
                >
                  {/* Nút xoá nằm TRONG nội dung, không dùng prop `actions`:
                      actions của AntD tràn ra ngoài thẻ khi item hẹp. */}
                  <Flex gap={6} align="flex-start" style={{ width: '100%', minWidth: 0 }}>
                    <Space direction="vertical" size={0} style={{ flex: 1, minWidth: 0 }}>
                      <Text strong style={{ fontSize: 13 }}>{fmtTime(d.created_at)}</Text>
                      <Space size={4} wrap>
                        <Tag color="blue">{scope(d)}</Tag>
                        <Text type="secondary" style={{ fontSize: 12 }}>{d.article_count} bài</Text>
                      </Space>
                      <Text type="secondary" style={{ fontSize: 12 }} ellipsis>
                        {d.preview}
                      </Text>
                    </Space>
                    <Popconfirm
                      title="Xoá bản điểm tin này?"
                      okText="Xoá"
                      cancelText="Huỷ"
                      onConfirm={(e) => {
                        e?.stopPropagation()
                        remove(d.id)
                      }}
                      onCancel={(e) => e?.stopPropagation()}
                    >
                      <Button
                        size="small"
                        type="text"
                        danger
                        icon={<DeleteOutlined />}
                        onClick={(e) => e.stopPropagation()}
                      />
                    </Popconfirm>
                  </Flex>
                </List.Item>
              )}
            />
          </Card>
        </Col>

        <Col span={17}>
          {running ? (
            <JobRunningCard
              label={running.label}
              elapsed={running.elapsed_sec}
              hint="AI thường mất 20–60 giây. Bạn có thể sang tab khác — bản điểm tin sẽ tự xuất hiện trong lịch sử khi xong."
            />
          ) : current ? (
            <Card
              size="small"
              title="Bản điểm tin"
              extra={
                <Text type="secondary" style={{ fontSize: 12 }}>
                  {fmtTime(current.created_at)} · {scope(current)} · từ {current.article_count} bài
                  {current.model ? ` · model: ${current.model}` : ''}
                </Text>
              }
            >
              <Md text={current.text ?? ''} truncated={current.truncated} />
            </Card>
          ) : (
            <Empty description="Chọn khoảng thời gian rồi bấm Viết điểm tin — AI sẽ tổng hợp Tin chính / Đáng chú ý / Xu hướng từ các bài đã thu thập. Mỗi bản đều được lưu lại ở cột bên trái." />
          )}
        </Col>
      </Row>
    </Space>
  )
}
