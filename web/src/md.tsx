// Minimal markdown renderer for AI output (headings, bullets, bold) — enough
// for điểm tin / tóm tắt without pulling in a full md library.

import React from 'react'
import { Alert } from 'antd'

function inline(text: string): React.ReactNode[] {
  const parts: React.ReactNode[] = []
  let rest = text
  let key = 0
  while (rest.length) {
    const m = rest.match(/\*\*(.+?)\*\*/)
    if (!m || m.index === undefined) {
      parts.push(rest)
      break
    }
    if (m.index > 0) parts.push(rest.slice(0, m.index))
    parts.push(<b key={key++}>{m[1]}</b>)
    rest = rest.slice(m.index + m[0].length)
  }
  return parts
}

/// `truncated` — the backend judged this reply cut off mid-thought (the model
/// hit its output cap). The partial answer is still shown, because half a
/// bản tin beats an error message, but it must be labelled as partial.
export function Md({ text, truncated }: { text: string; truncated?: boolean }) {
  const blocks: React.ReactNode[] = []
  let list: string[] = []
  let key = 0
  const flush = () => {
    if (list.length) {
      blocks.push(
        <ul key={key++}>
          {list.map((li, i) => (
            <li key={i}>{inline(li)}</li>
          ))}
        </ul>,
      )
      list = []
    }
  }
  for (const raw of text.split('\n')) {
    const line = raw.trimEnd()
    if (/^\s*[-*•]\s+/.test(line)) {
      list.push(line.replace(/^\s*[-*•]\s+/, ''))
      continue
    }
    flush()
    if (line.startsWith('### ')) blocks.push(<h3 key={key++}>{inline(line.slice(4))}</h3>)
    else if (line.startsWith('## ')) blocks.push(<h2 key={key++}>{inline(line.slice(3))}</h2>)
    else if (line.startsWith('# ')) blocks.push(<h2 key={key++}>{inline(line.slice(2))}</h2>)
    else if (line.trim().length) blocks.push(<p key={key++}>{inline(line)}</p>)
  }
  flush()
  return (
    <div className="md-body">
      {blocks}
      {truncated && (
        <Alert
          type="warning"
          showIcon
          style={{ marginTop: 10 }}
          message="Nội dung bị cắt giữa chừng"
          description="Model dừng trước khi viết xong. Bấm chạy lại, hoặc thu hẹp khoảng thời gian / dữ liệu đầu vào."
        />
      )}
    </div>
  )
}
