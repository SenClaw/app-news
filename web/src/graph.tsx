// Bản đồ liên kết dòng sự kiện — force-directed layout tính một lần (thuần
// TS, deterministic, không thêm thư viện), sau đó người dùng tự do kéo node,
// kéo nền để di chuyển và cuộn để phóng to/thu nhỏ.
//
// Node = dòng sự kiện (to theo số bài), cạnh = hai sự kiện cùng mạch chuyện
// (đậm theo mức trùng cụm từ).
//
// Ba thứ quyết định bản đồ có đọc được hay không, học từ lần đầu vẽ ra một cục
// rối 60 node / 467 cạnh:
//
//   1. Bố cục theo TỪNG CỤM LIÊN THÔNG rồi xếp cạnh nhau, thay vì thả tất cả
//      vào một hệ lực chung — khi đó cụm đông nhất hút hết mọi thứ vào giữa
//      còn các node lẻ dạt ra rìa, bỏ phí gần hết khung.
//   2. Lực hút CHIA CHO BẬC của node: không thế thì node nhiều liên kết kéo
//      sập cả vùng quanh nó thành một chấm.
//   3. Nhãn chỉ vẽ khi CHỖ ĐÓ CÒN TRỐNG (xét va chạm thật, node to trước),
//      chứ không theo ngưỡng zoom — ngưỡng zoom cho ra 40 nhãn chồng lên nhau.

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  Alert, Button, Card, Empty, Flex, Input, List, Select, Slider, Space, Spin, Tag, Tooltip, Typography, message,
} from 'antd'
import {
  AimOutlined, CloseOutlined, RobotOutlined, SearchOutlined, ZoomInOutlined, ZoomOutOutlined,
} from '@ant-design/icons'
import { api, type GraphAnalysis, type StoryGraphData } from './api'
import { JobRunningCard } from './jobs'
import { Md } from './md'

const { Text } = Typography

/** Palette for AI-detected story threads (mạch chuyện). */
const CLUSTER_COLORS = ['#f59e0b', '#a855f7', '#22c55e', '#ef4444', '#14b8a6', '#eab308', '#ec4899', '#8b5cf6']

interface LaidNode {
  id: number
  title: string
  count: number
  x: number
  y: number
  r: number
  /** Connected component this node belongs to (index into the packed groups). */
  group: number
  /** How many links it has after pruning — drives label priority. */
  degree: number
}

const nodeRadius = (count: number) => Math.min(30, 10 + Math.sqrt(count) * 4.5)

/** Split the graph into connected components, biggest first. */
function components(ids: number[], edges: { a: number; b: number }[]): number[][] {
  const adj = new Map<number, number[]>()
  for (const id of ids) adj.set(id, [])
  for (const e of edges) {
    if (!adj.has(e.a) || !adj.has(e.b)) continue
    adj.get(e.a)!.push(e.b)
    adj.get(e.b)!.push(e.a)
  }
  const seen = new Set<number>()
  const out: number[][] = []
  for (const id of ids) {
    if (seen.has(id)) continue
    const comp: number[] = []
    const stack = [id]
    seen.add(id)
    while (stack.length) {
      const cur = stack.pop()!
      comp.push(cur)
      for (const nb of adj.get(cur) ?? []) {
        if (!seen.has(nb)) {
          seen.add(nb)
          stack.push(nb)
        }
      }
    }
    out.push(comp)
  }
  return out.sort((a, b) => b.length - a.length)
}

/** Force-directed layout of ONE component, centred on (0,0). */
function layoutComponent(
  ids: number[],
  sizeOf: Map<number, number>,
  edges: { a: number; b: number; weight: number }[],
): { pos: Map<number, { x: number; y: number }>; radius: number } {
  const n = ids.length
  const pos = new Map<number, { x: number; y: number }>()
  if (n === 1) {
    pos.set(ids[0], { x: 0, y: 0 })
    return { pos, radius: sizeOf.get(ids[0])! + 8 }
  }

  // Spring length comes from how big the CIRCLES are, not from how big a
  // canvas we felt like giving them. Deriving it from an arbitrary radius blew
  // the map up to 5000 units wide, at which point every node rendered ~6px and
  // nothing was readable.
  const avgR = ids.reduce((s, id) => s + sizeOf.get(id)!, 0) / n
  const k = avgR * 3.6
  const R = (k * Math.sqrt(n)) / 1.8

  const P = ids.map((id, i) => {
    const angle = i * 2.399963 // golden angle — even spread, no empty middle
    const rad = R * Math.sqrt((i + 0.5) / n)
    return { id, x: rad * Math.cos(angle), y: rad * Math.sin(angle), r: sizeOf.get(id)! }
  })
  const idx = new Map(P.map((p, i) => [p.id, i]))

  // Degree is used to damp attraction: a hub linked to a dozen others would
  // otherwise be pulled by a dozen springs at once and drag them all onto it.
  const deg = new Map<number, number>()
  for (const e of edges) {
    deg.set(e.a, (deg.get(e.a) ?? 0) + 1)
    deg.set(e.b, (deg.get(e.b) ?? 0) + 1)
  }

  let temp = R / 4
  const iters = n > 60 ? 220 : 340
  for (let it = 0; it < iters; it++) {
    const dx = new Array(n).fill(0)
    const dy = new Array(n).fill(0)
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        let vx = P[i].x - P[j].x
        let vy = P[i].y - P[j].y
        let d2 = vx * vx + vy * vy
        if (d2 < 0.01) {
          vx = ((i * 13) % 7) - 3
          vy = ((j * 17) % 7) - 3
          d2 = vx * vx + vy * vy
        }
        const d = Math.sqrt(d2)
        const f = (k * k) / d
        dx[i] += (vx / d) * f
        dy[i] += (vy / d) * f
        dx[j] -= (vx / d) * f
        dy[j] -= (vy / d) * f
      }
    }
    for (const e of edges) {
      const i = idx.get(e.a)
      const j = idx.get(e.b)
      if (i === undefined || j === undefined) continue
      const vx = P[i].x - P[j].x
      const vy = P[i].y - P[j].y
      const d = Math.sqrt(vx * vx + vy * vy) || 0.1
      const damp = 1 / Math.sqrt(Math.max(deg.get(e.a)!, deg.get(e.b)!))
      const f = ((d * d) / k) * (0.25 + e.weight * 0.75) * damp
      dx[i] -= (vx / d) * f
      dy[i] -= (vy / d) * f
      dx[j] += (vx / d) * f
      dy[j] += (vy / d) * f
    }
    for (let i = 0; i < n; i++) {
      dx[i] += -P[i].x * 0.04 // gentle pull to the component's own centre
      dy[i] += -P[i].y * 0.04
      const d = Math.hypot(dx[i], dy[i]) || 0.1
      const step = Math.min(d, temp)
      P[i].x += (dx[i] / d) * step
      P[i].y += (dy[i] / d) * step
    }
    temp *= 0.985
  }

  // Forces settle the SHAPE, not the scale: repulsion grows with n while the
  // centring pull does not, so a 50-node component drifted out to a ~2000-unit
  // radius and the whole sheet had to be shrunk to 22% to fit — every node came
  // out three pixels across. Rescale to the area the circles genuinely need
  // (≈55% packing density) and keep the shape the simulation found.
  let spread = 0
  for (const p of P) spread = Math.max(spread, Math.hypot(p.x, p.y))
  const target = Math.sqrt((n * (avgR + 18) ** 2) / 0.55)
  if (spread > 1) {
    const s = target / spread
    for (const p of P) {
      p.x *= s
      p.y *= s
    }
  }

  // Circles must never sit on top of each other — the forces get close, this
  // makes it exact, and it undoes any crowding the rescale introduced.
  for (let pass = 0; pass < 120; pass++) {
    let moved = false
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        const need = P[i].r + P[j].r + 16
        let vx = P[j].x - P[i].x
        let vy = P[j].y - P[i].y
        let d = Math.hypot(vx, vy)
        if (d === 0) {
          vx = 1
          vy = 0
          d = 1
        }
        if (d < need) {
          const push = (need - d) / 2
          P[i].x -= (vx / d) * push
          P[i].y -= (vy / d) * push
          P[j].x += (vx / d) * push
          P[j].y += (vy / d) * push
          moved = true
        }
      }
    }
    if (!moved) break
  }

  let radius = 0
  for (const p of P) {
    pos.set(p.id, { x: p.x, y: p.y })
    radius = Math.max(radius, Math.hypot(p.x, p.y) + p.r)
  }
  return { pos, radius: radius + 10 }
}

/**
 * Lay out every component, then pack them left-to-right in rows.
 *
 * Packing is what reclaims the canvas: the screenshot that started this rewrite
 * had one crushed blob in the middle and eight lonely circles floating in empty
 * space, because a single global simulation has no way to place unconnected
 * things sensibly.
 */
function layout(data: StoryGraphData): { nodes: LaidNode[]; W: number; H: number } {
  const n = data.nodes.length
  if (n === 0) return { nodes: [], W: 1000, H: 620 }

  const sizeOf = new Map(data.nodes.map((nd) => [nd.id, nodeRadius(nd.article_count)]))
  const meta = new Map(data.nodes.map((nd) => [nd.id, nd]))
  const ids = data.nodes.map((nd) => nd.id)
  const comps = components(ids, data.edges)
  const degree = new Map<number, number>()
  for (const e of data.edges) {
    degree.set(e.a, (degree.get(e.a) ?? 0) + 1)
    degree.set(e.b, (degree.get(e.b) ?? 0) + 1)
  }

  const laidComps = comps.map((comp) => {
    const inside = new Set(comp)
    const sub = data.edges.filter((e) => inside.has(e.a) && inside.has(e.b))
    return { comp, ...layoutComponent(comp, sizeOf, sub) }
  })

  // Row packing, aiming for a landscape sheet: total area × 16/9, so the result
  // fills a screen rather than becoming a tall column.
  const gap = 46
  const area = laidComps.reduce((s, c) => s + (c.radius * 2 + gap) ** 2, 0)
  const rowWidth = Math.max(900, laidComps[0].radius * 2 + gap, Math.sqrt(area * (16 / 9)))
  const nodes: LaidNode[] = []
  let cursorX = gap
  let rowY = gap
  let rowH = 0
  let W = 0
  for (let gi = 0; gi < laidComps.length; gi++) {
    const c = laidComps[gi]
    const d = c.radius * 2
    if (cursorX > gap && cursorX + d > rowWidth) {
      cursorX = gap
      rowY += rowH + gap
      rowH = 0
    }
    const cx = cursorX + c.radius
    const cy = rowY + c.radius
    for (const id of c.comp) {
      const p = c.pos.get(id)!
      const nd = meta.get(id)!
      nodes.push({
        id,
        title: nd.title,
        count: nd.article_count,
        x: cx + p.x,
        y: cy + p.y,
        r: sizeOf.get(id)!,
        group: gi,
        degree: degree.get(id) ?? 0,
      })
    }
    cursorX += d + gap
    rowH = Math.max(rowH, d)
    W = Math.max(W, cursorX)
  }
  return { nodes, W: Math.round(W + gap), H: Math.round(rowY + rowH + gap * 2) }
}

/** Fixed drawing surface. The map is transformed into it by zoom/pan. */
const VIEW = { W: 1400, H: 760 }

const short = (s: string, n = 26) => (s.length > n ? s.slice(0, n) + '…' : s)

/**
 * Decide which labels to draw so none overlaps another.
 *
 * Greedy, biggest-and-best-connected first: reserve each label's box and skip
 * any later label whose box would hit one already taken. Cheap (n² on ≤150
 * nodes) and gives a stable, readable result at every zoom level — unlike the
 * old "show it if zoom ≥ 1.5", which drew every label at once and let them pile
 * up into the unreadable smudge in the middle of the map.
 */
function placeLabels(
  nodes: LaidNode[],
  forced: Set<number>,
  zoom: number,
  chars: number,
): Map<number, number> {
  const CHAR_W = 5.6
  const LINE_H = 13
  const byRank = (a: LaidNode, b: LaidNode) => b.count - a.count || b.degree - a.degree || a.id - b.id
  // Forced labels (pinned / searched / hovered) claim their spot first; the
  // rest fill whatever room is left.
  const order = [
    ...nodes.filter((nd) => forced.has(nd.id)).sort(byRank),
    ...nodes.filter((nd) => !forced.has(nd.id)).sort(byRank),
  ]
  const taken: { x1: number; y1: number; x2: number; y2: number }[] = []
  const out = new Map<number, number>()
  const budget = Math.round(14 * Math.max(1, zoom))

  for (const nd of order) {
    const must = forced.has(nd.id)
    if (!must && out.size - forced.size >= budget) continue
    const w = Math.min(nd.title.length, chars) * CHAR_W
    // Below the circle first (the natural place), then above, then a line lower
    // — enough freedom that two neighbouring nodes can both keep their label.
    const options = [nd.r + 13, -(nd.r + 5), nd.r + 26]
    let placed: number | null = null
    for (const dy of options) {
      const top = nd.y + dy - LINE_H + 3
      const box = { x1: nd.x - w / 2, y1: top, x2: nd.x + w / 2, y2: top + LINE_H }
      // Circles are obstacles too: a label must not land on another bubble.
      const hitsNode = nodes.some(
        (o) =>
          o.id !== nd.id &&
          Math.abs(o.x - nd.x) < w / 2 + o.r &&
          Math.abs(o.y - (box.y1 + LINE_H / 2)) < o.r,
      )
      const hits =
        hitsNode || taken.some((t) => !(box.x2 < t.x1 || box.x1 > t.x2 || box.y2 < t.y1 || box.y1 > t.y2))
      if (!hits) {
        taken.push(box)
        placed = dy
        break
      }
    }
    // A forced label is shown even with nowhere clean to sit — the reader asked
    // for that one specifically; an auto label just steps aside.
    if (placed === null && must) placed = nd.r + 13
    if (placed !== null) out.set(nd.id, placed)
  }

  // Final guarantee. The search above tries a few slots and can still leave two
  // labels sharing a line (a forced label that found no clean slot always
  // takes one). Here every remaining collision is resolved outright by pushing
  // the lower label down — nothing is left overlapping, whatever happened above.
  const placedList = [...out.entries()]
    .map(([id, dy]) => {
      const nd = nodes.find((v) => v.id === id)!
      const w = Math.min(nd.title.length, chars) * CHAR_W
      return { id, nd, w, dy }
    })
    .sort((a, b) => a.nd.y + a.dy - (b.nd.y + b.dy))
  for (let pass = 0; pass < 6; pass++) {
    let moved = false
    for (let i = 0; i < placedList.length; i++) {
      for (let j = i + 1; j < placedList.length; j++) {
        const A = placedList[i]
        const B = placedList[j]
        const ax = Math.abs(A.nd.x - B.nd.x)
        if (ax > (A.w + B.w) / 2) continue // no horizontal overlap, no problem
        const ay = A.nd.y + A.dy
        const by = B.nd.y + B.dy
        if (Math.abs(ay - by) >= LINE_H + 2) continue
        B.dy += LINE_H + 2 - (by - ay) // push the lower one onto its own line
        moved = true
      }
    }
    if (!moved) break
  }
  for (const p of placedList) out.set(p.id, p.dy)
  return out
}

export default function StoryGraph({
  days,
  minArticles,
  onSelect,
}: {
  days: number
  minArticles: number
  onSelect: (storyId: number) => void
}) {
  const [data, setData] = useState<StoryGraphData | null>(null)
  const [nodes, setNodes] = useState<LaidNode[]>([])
  const [box, setBox] = useState(VIEW)
  const [hover, setHover] = useState<number | null>(null)
  /** Pinned node: clicking one keeps its neighbourhood highlighted and opens
   *  the side panel, instead of jumping straight to the timeline. */
  const [focus, setFocus] = useState<number | null>(null)
  const [perNode, setPerNode] = useState(3)
  const [q, setQ] = useState('')
  const [zoom, setZoom] = useState(1)
  const [pan, setPan] = useState({ x: 0, y: 0 })
  const [ai, setAi] = useState<GraphAnalysis | null>(null)
  const [aiBusy, setAiBusy] = useState(false)
  const [aiSec, setAiSec] = useState(0)
  const svgRef = useRef<SVGSVGElement | null>(null)
  /** Active pointer gesture: dragging a node, or panning the canvas. */
  const drag = useRef<
    | { kind: 'node'; id: number; dx: number; dy: number }
    | { kind: 'pan'; x0: number; y0: number; px: number; py: number }
    | null
  >(null)
  /** Set once a gesture actually moved, so a drag doesn't fire the node's
   *  click handler — pointerup clears `drag` before click runs. */
  const moved = useRef(false)

  useEffect(() => {
    setData(null)
    setAi(null)
    setFocus(null)
    api.storyGraph(days, minArticles, perNode).then(setData)
  }, [days, minArticles, perNode])

  const runAi = async () => {
    setAiBusy(true)
    setAiSec(0)
    const started = Date.now()
    const ticker = window.setInterval(() => setAiSec(Math.round((Date.now() - started) / 1000)), 1000)
    try {
      const r = await api.analyzeGraph(days, minArticles)
      if (r.error) message.error(String(r.error))
      else setAi(r)
    } finally {
      window.clearInterval(ticker)
      setAiBusy(false)
    }
  }

  const laid = useMemo(() => (data ? layout(data) : null), [data])

  /** Fit the laid-out sheet into the viewport: zoom + centring pan. */
  const fitView = useCallback((l: { W: number; H: number } | null) => {
    if (!l) return { zoom: 1, pan: { x: 0, y: 0 } }
    const z = Math.max(0.2, Math.min(1, VIEW.W / l.W, VIEW.H / l.H))
    return { zoom: z, pan: { x: (VIEW.W - l.W * z) / 2, y: (VIEW.H - l.H * z) / 2 } }
  }, [])

  useEffect(() => {
    if (laid) {
      setNodes(laid.nodes)
      // The drawing surface is FIXED and the sheet is transformed into it. With
      // a viewBox that grew with the layout instead, a busy week produced a
      // 5000-unit-wide box squeezed into the page and every node came out ~6px.
      setBox(VIEW)
      const f = fitView(laid)
      setZoom(f.zoom)
      setPan(f.pan)
    }
  }, [laid, fitView])

  /** Screen px → SVG user units (viewBox space), independent of zoom/pan. */
  const toLocal = useCallback(
    (clientX: number, clientY: number) => {
      const rect = svgRef.current?.getBoundingClientRect()
      if (!rect) return { x: 0, y: 0 }
      const scale = box.W / rect.width
      return {
        x: ((clientX - rect.left) * scale - pan.x) / zoom,
        y: ((clientY - rect.top) * scale - pan.y) / zoom,
      }
    },
    [box.W, pan.x, pan.y, zoom],
  )

  const onPointerDownNode = (e: React.PointerEvent, nd: LaidNode) => {
    e.stopPropagation()
    ;(e.target as Element).setPointerCapture?.(e.pointerId)
    const p = toLocal(e.clientX, e.clientY)
    moved.current = false
    drag.current = { kind: 'node', id: nd.id, dx: p.x - nd.x, dy: p.y - nd.y }
  }

  const onPointerDownCanvas = (e: React.PointerEvent) => {
    ;(e.currentTarget as Element).setPointerCapture?.(e.pointerId)
    moved.current = false
    drag.current = { kind: 'pan', x0: e.clientX, y0: e.clientY, px: pan.x, py: pan.y }
  }

  const onPointerMove = (e: React.PointerEvent) => {
    const d = drag.current
    if (!d) return
    if (d.kind === 'node') {
      const p = toLocal(e.clientX, e.clientY)
      const target = nodes.find((v) => v.id === d.id)
      if (target && Math.hypot(p.x - d.dx - target.x, p.y - d.dy - target.y) > 2) moved.current = true
      setNodes((prev) => prev.map((v) => (v.id === d.id ? { ...v, x: p.x - d.dx, y: p.y - d.dy } : v)))
    } else {
      if (Math.hypot(e.clientX - d.x0, e.clientY - d.y0) > 3) moved.current = true
      const rect = svgRef.current?.getBoundingClientRect()
      const scale = rect ? box.W / rect.width : 1
      setPan({ x: d.px + (e.clientX - d.x0) * scale, y: d.py + (e.clientY - d.y0) * scale })
    }
  }

  const endDrag = () => {
    drag.current = null
  }

  // Wheel zooms toward the cursor so the point under the pointer stays put.
  const onWheel = (e: React.WheelEvent) => {
    const rect = svgRef.current?.getBoundingClientRect()
    if (!rect) return
    const scale = box.W / rect.width
    const cx = (e.clientX - rect.left) * scale
    const cy = (e.clientY - rect.top) * scale
    const factor = e.deltaY < 0 ? 1.12 : 1 / 1.12
    const next = Math.min(4, Math.max(0.3, zoom * factor))
    setPan({ x: cx - ((cx - pan.x) / zoom) * next, y: cy - ((cy - pan.y) / zoom) * next })
    setZoom(next)
  }

  const reset = () => {
    if (laid) setNodes(laid.nodes.map((v) => ({ ...v })))
    const f = fitView(laid)
    setZoom(f.zoom)
    setPan(f.pan)
  }

  const zoomBy = (f: number) => {
    const next = Math.min(4, Math.max(0.3, zoom * f))
    const cx = box.W / 2
    const cy = box.H / 2
    setPan({ x: cx - ((cx - pan.x) / zoom) * next, y: cy - ((cy - pan.y) / zoom) * next })
    setZoom(next)
  }

  const pos = useMemo(() => new Map(nodes.map((nd) => [nd.id, nd])), [nodes])

  if (!data) return <Spin style={{ marginTop: 40 }} />
  if (nodes.length === 0)
    return <Empty description="Chưa có dòng sự kiện nào trong cửa sổ này — thu thập thêm tin trước" />

  // Màu node theo mạch chuyện AI gom được (id → màu cụm).
  const clusterOf = new Map<number, { color: string; name: string }>()
  ai?.clusters?.forEach((c, i) => {
    const color = CLUSTER_COLORS[i % CLUSTER_COLORS.length]
    for (const id of c.story_ids) clusterOf.set(id, { color, name: c.name })
  })
  const noiseKey = new Set((ai?.noise ?? []).map((l) => `${Math.min(l.a, l.b)}-${Math.max(l.a, l.b)}`))

  // Whatever the pointer is on wins; otherwise the pinned node stays lit, so
  // the neighbourhood you clicked survives the mouse moving away.
  const active = hover ?? focus
  const neighbors = new Set<number>()
  if (active !== null) {
    neighbors.add(active)
    for (const e of data.edges) {
      if (e.a === active) neighbors.add(e.b)
      if (e.b === active) neighbors.add(e.a)
    }
    for (const l of ai?.ai_links ?? []) {
      if (l.a === active) neighbors.add(l.b)
      if (l.b === active) neighbors.add(l.a)
    }
  }
  const dimmed = (id: number) => active !== null && !neighbors.has(id)

  const needle = q.trim().toLowerCase()
  const matches = needle
    ? new Set(nodes.filter((nd) => nd.title.toLowerCase().includes(needle)).map((nd) => nd.id))
    : null

  // Labels: whatever is lit or searched must show; the rest fill the gaps.
  const forced = new Set<number>()
  if (active !== null) for (const id of neighbors) forced.add(id)
  if (matches) for (const id of matches) forced.add(id)
  const labelDy = placeLabels(nodes, forced, zoom, 26)

  const focusNode = focus !== null ? nodes.find((nd) => nd.id === focus) : undefined
  const focusLinks =
    focus === null
      ? []
      : data.edges
          .filter((e) => e.a === focus || e.b === focus)
          .map((e) => ({ other: e.a === focus ? e.b : e.a, weight: e.weight, shared: e.shared }))
          .sort((a, b) => b.weight - a.weight)

  return (
    <div>
      <Flex align="center" justify="space-between" wrap gap={8} style={{ marginBottom: 6 }}>
        <Space size={8} wrap>
          <Input
            size="small"
            allowClear
            prefix={<SearchOutlined />}
            placeholder="Tìm sự kiện trên bản đồ"
            value={q}
            onChange={(e) => {
              setQ(e.target.value)
              if (e.target.value.trim()) setFocus(null)
            }}
            style={{ width: 210 }}
          />
          <Tooltip title="Mỗi sự kiện giữ lại bao nhiêu liên kết mạnh nhất. Càng nhiều càng đầy đủ nhưng càng rối.">
            <Space size={4}>
              <Text type="secondary" style={{ fontSize: 12 }}>
                Độ dày liên kết
              </Text>
              <Select
                size="small"
                value={perNode}
                onChange={setPerNode}
                style={{ width: 132 }}
                options={[
                  { value: 2, label: 'Gọn (2/sự kiện)' },
                  { value: 3, label: 'Vừa (3/sự kiện)' },
                  { value: 5, label: 'Nhiều (5/sự kiện)' },
                  { value: 0, label: 'Tất cả (rối)' },
                ]}
              />
            </Space>
          </Tooltip>
        </Space>
        <Space size={6}>
          <Tooltip title="Thu nhỏ">
            <Button size="small" icon={<ZoomOutOutlined />} onClick={() => zoomBy(1 / 1.25)} />
          </Tooltip>
          <Slider
            min={0.3}
            max={4}
            step={0.05}
            value={zoom}
            onChange={setZoom}
            style={{ width: 110 }}
            tooltip={{ formatter: (v) => `${Math.round((v ?? 1) * 100)}%` }}
          />
          <Tooltip title="Phóng to">
            <Button size="small" icon={<ZoomInOutlined />} onClick={() => zoomBy(1.25)} />
          </Tooltip>
          <Tooltip title="Về bố cục gốc">
            <Button size="small" icon={<AimOutlined />} onClick={reset} />
          </Tooltip>
          <Button size="small" type="primary" icon={<RobotOutlined />} loading={aiBusy} onClick={runAi}>
            AI liên kết & phân tích
          </Button>
        </Space>
      </Flex>

      {aiBusy && (
        <div style={{ marginBottom: 8 }}>
          <JobRunningCard label="Đang phân tích liên kết giữa các sự kiện" elapsed={aiSec} />
        </div>
      )}

      {!aiBusy && ai && (
        <Alert
          type="info"
          style={{ marginBottom: 8 }}
          message={
            <Space size={[6, 6]} wrap>
              <Text strong>AI đã map lại bản đồ</Text>
              {ai.clusters?.map((c, i) => (
                <Tag key={c.name} color={CLUSTER_COLORS[i % CLUSTER_COLORS.length]}>
                  {c.name} ({c.story_ids.length})
                </Tag>
              ))}
              {ai.ai_links?.length ? <Tag color="purple">+{ai.ai_links.length} liên kết AI nối thêm</Tag> : null}
              {ai.noise?.length ? <Tag color="red">{ai.noise.length} liên kết nghi nhiễu</Tag> : null}
            </Space>
          }
          description={ai.summary ? <Md text={ai.summary} /> : undefined}
          closable
          onClose={() => setAi(null)}
        />
      )}

      <svg
        ref={svgRef}
        viewBox={`0 0 ${box.W} ${box.H}`}
        style={{
          width: '100%',
          aspectRatio: `${VIEW.W} / ${VIEW.H}`,
          background: 'var(--news-graph-bg)',
          borderRadius: 12,
          cursor: drag.current?.kind === 'pan' ? 'grabbing' : 'grab',
          touchAction: 'none',
          // Dragging must move the map, not paint a text selection over it.
          userSelect: 'none',
          WebkitUserSelect: 'none',
        }}
        onPointerDown={onPointerDownCanvas}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerLeave={endDrag}
        onWheel={onWheel}
      >
        <g transform={`translate(${pan.x},${pan.y}) scale(${zoom})`}>
          {data.edges.map((e, i) => {
            const a = pos.get(e.a)
            const b = pos.get(e.b)
            if (!a || !b) return null
            const lit = active === e.a || active === e.b
            const isNoise = noiseKey.has(`${Math.min(e.a, e.b)}-${Math.max(e.a, e.b)}`)
            return (
              <g key={i}>
                <line
                  x1={a.x}
                  y1={a.y}
                  x2={b.x}
                  y2={b.y}
                  stroke={isNoise ? '#ef4444' : lit ? '#0ea5e9' : 'var(--news-edge)'}
                  strokeWidth={(1 + e.weight * 4) / Math.max(1, zoom * 0.8)}
                  strokeOpacity={
                    matches ? 0.05 : isNoise ? 0.35 : active === null ? 0.45 : lit ? 0.95 : 0.05
                  }
                  strokeDasharray={isNoise ? '2 4' : undefined}
                />
                <title>
                  {`chung: ${e.shared.join(', ')} (${Math.round(e.weight * 100)}%)` +
                    (isNoise ? ' — AI cho rằng đây chỉ là trùng từ, không cùng chuyện' : '')}
                </title>
              </g>
            )
          })}
          {/* Liên kết ngữ nghĩa do AI nối thêm — nét đứt tím, máy không thấy. */}
          {(ai?.ai_links ?? []).map((l, i) => {
            const a = pos.get(l.a)
            const b = pos.get(l.b)
            if (!a || !b) return null
            const lit = active === l.a || active === l.b
            return (
              <g key={`ai-${i}`}>
                <line
                  x1={a.x}
                  y1={a.y}
                  x2={b.x}
                  y2={b.y}
                  stroke="#a855f7"
                  strokeWidth={2 / Math.max(1, zoom * 0.8)}
                  strokeOpacity={active === null ? 0.75 : lit ? 1 : 0.1}
                  strokeDasharray="6 4"
                />
                <title>{`AI: ${l.relation || 'liên quan'} — ${l.why}`}</title>
              </g>
            )
          })}
          {nodes.map((nd) => {
            const color = clusterOf.get(nd.id)?.color ?? '#0ea5e9'
            const lit = active === nd.id
            const hit = matches?.has(nd.id)
            return (
              <g
                key={nd.id}
                transform={`translate(${nd.x},${nd.y})`}
                style={{ cursor: 'pointer' }}
                // While searching, the search decides what is visible — otherwise
                // a hit outside the pinned neighbourhood got dimmed by BOTH and
                // the thing you looked for was the hardest thing to see.
                opacity={matches ? (hit ? 1 : 0.1) : dimmed(nd.id) ? 0.15 : 1}
                onPointerDown={(e) => onPointerDownNode(e, nd)}
                onPointerEnter={() => setHover(nd.id)}
                onPointerLeave={() => setHover(null)}
                onClick={() => {
                  // A click PINS the neighbourhood and opens the panel. Jumping
                  // straight to the timeline made the map a one-way door: you
                  // lost the map the instant you touched it.
                  if (!moved.current) setFocus((f) => (f === nd.id ? null : nd.id))
                }}
                onDoubleClick={() => onSelect(nd.id)}
              >
                {(lit || hit) && <circle r={nd.r + 6} fill="none" stroke={color} strokeWidth={2} strokeOpacity={0.5} />}
                <circle
                  r={nd.r}
                  fill={color}
                  fillOpacity={lit ? 0.6 : 0.3}
                  stroke={color}
                  strokeWidth={clusterOf.has(nd.id) ? 2.5 : 1.5}
                />
                <text textAnchor="middle" dy={4} fontSize={11} fill="var(--news-node-value)">
                  {nd.count}
                </text>
                {labelDy.has(nd.id) && (
                  <text
                    textAnchor="middle"
                    dy={labelDy.get(nd.id)}
                    fontSize={11}
                    fill="var(--news-node-label)"
                    // Halo: labels sit over edges, and a hairline of the board
                    // colour behind the glyphs keeps them readable there.
                    stroke="var(--news-graph-bg)"
                    strokeWidth={3}
                    paintOrder="stroke"
                    style={{ pointerEvents: 'none' }}
                  >
                    {short(nd.title)}
                  </text>
                )}
                <title>
                  {`${nd.title} — ${nd.count} bài` +
                    (clusterOf.get(nd.id) ? `\nMạch: ${clusterOf.get(nd.id)!.name}` : '') +
                    '\nBấm để xem liên kết · bấm đúp để mở diễn biến'}
                </title>
              </g>
            )
          })}
        </g>
      </svg>

      {focusNode && (
        <Card
          size="small"
          style={{ marginTop: 10 }}
          title={
            <Space size={6} wrap>
              <Tag color="blue">{focusNode.count} bài</Tag>
              <Text strong>{focusNode.title}</Text>
            </Space>
          }
          extra={
            <Space size={6}>
              <Button size="small" type="primary" onClick={() => onSelect(focusNode.id)}>
                Xem diễn biến
              </Button>
              <Button size="small" icon={<CloseOutlined />} onClick={() => setFocus(null)} />
            </Space>
          }
        >
          {focusLinks.length === 0 ? (
            <Text type="secondary" style={{ fontSize: 12 }}>
              Sự kiện này đứng riêng — không chung cụm từ khóa nào với các sự kiện khác trong cửa sổ đang xem.
            </Text>
          ) : (
            <List
              size="small"
              dataSource={focusLinks}
              renderItem={(l) => (
                <List.Item style={{ paddingLeft: 0, paddingRight: 0 }}>
                  <Space direction="vertical" size={0} style={{ width: '100%' }}>
                    <Space size={6} wrap>
                      <a onClick={() => setFocus(l.other)}>{short(pos.get(l.other)?.title ?? `#${l.other}`, 46)}</a>
                      <Tag>{Math.round(l.weight * 100)}%</Tag>
                    </Space>
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      chung: {l.shared.join(', ')}
                    </Text>
                  </Space>
                </List.Item>
              )}
            />
          )}
        </Card>
      )}

      <Space size={6} style={{ marginTop: 6 }} wrap>
        <Tag color="blue">
          {nodes.length} sự kiện · {data.edges.length} liên kết đang vẽ
        </Tag>
        {data.edges_hidden > 0 && (
          <Tooltip title="Liên kết yếu hơn của những sự kiện đã đủ số liên kết mạnh. Chọn 'Tất cả' ở Độ dày liên kết để xem hết.">
            <Tag>ẩn {data.edges_hidden} liên kết yếu / tổng {data.edges_total}</Tag>
          </Tooltip>
        )}
        {ai?.ai_links?.length ? (
          <Tag color="purple">
            <span style={{ borderBottom: '2px dashed #a855f7', paddingBottom: 1 }}>nét đứt tím</span> = AI nối thêm
          </Tag>
        ) : null}
        {ai?.noise?.length ? (
          <Tag color="red">
            <span style={{ borderBottom: '2px dotted #ef4444', paddingBottom: 1 }}>nét chấm đỏ</span> = nghi nhiễu
          </Tag>
        ) : null}
        <Text type="secondary" style={{ fontSize: 12 }}>
          Bấm node để xem liên kết · bấm đúp để mở diễn biến · phóng to để hiện thêm nhãn.
        </Text>
      </Space>

      {ai && (ai.clusters?.length || ai.ai_links?.length) ? (
        <Card size="small" title="AI liên kết thông tin" style={{ marginTop: 12 }}>
          {ai.clusters?.length ? (
            <List
              size="small"
              header={<Text strong>Mạch chuyện</Text>}
              dataSource={ai.clusters}
              renderItem={(c, i) => (
                <List.Item>
                  <Space direction="vertical" size={2} style={{ width: '100%' }}>
                    <Space size={6} wrap>
                      <Tag color={CLUSTER_COLORS[i % CLUSTER_COLORS.length]}>{c.name}</Tag>
                      {c.story_ids.map((id) => (
                        <a key={id} onClick={() => onSelect(id)}>
                          {short(pos.get(id)?.title ?? `#${id}`, 34)}
                        </a>
                      ))}
                    </Space>
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      {c.why}
                    </Text>
                  </Space>
                </List.Item>
              )}
            />
          ) : null}
          {ai.ai_links?.length ? (
            <List
              size="small"
              header={<Text strong>Liên kết AI nối thêm (máy không thấy vì không trùng từ)</Text>}
              dataSource={ai.ai_links}
              renderItem={(l) => (
                <List.Item>
                  <Space direction="vertical" size={0} style={{ width: '100%' }}>
                    <Space size={6} wrap>
                      <a onClick={() => onSelect(l.a)}>{short(pos.get(l.a)?.title ?? `#${l.a}`, 30)}</a>
                      <Tag color="purple">{l.relation || 'liên quan'}</Tag>
                      <a onClick={() => onSelect(l.b)}>{short(pos.get(l.b)?.title ?? `#${l.b}`, 30)}</a>
                    </Space>
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      {l.why}
                    </Text>
                  </Space>
                </List.Item>
              )}
            />
          ) : null}
          <Text type="secondary" style={{ fontSize: 12 }}>
            Liên kết từ khóa do máy thống kê; phần gom mạch và nối thêm là nhận định AI — chỉ để tham khảo.
          </Text>
        </Card>
      ) : null}
    </div>
  )
}
