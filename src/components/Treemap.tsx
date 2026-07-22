import { useMemo } from "react";
import { formatBytes } from "../format";
import type { DeepEntry } from "../types";

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
  entry: DeepEntry;
}

/// Recursive binary split: the item list is cut into two halves of roughly
/// equal total size, and the rectangle is cut along its longer edge in the same
/// proportion. Splitting the long edge is what keeps blocks near-square, which
/// is what makes areas comparable by eye.
function layout(items: DeepEntry[], x: number, y: number, w: number, h: number, out: Rect[]) {
  if (items.length === 0 || w <= 0 || h <= 0) return;
  if (items.length === 1) {
    out.push({ x, y, w, h, entry: items[0] });
    return;
  }

  const total = items.reduce((a, i) => a + i.size_bytes, 0);
  if (total <= 0) return;

  let acc = 0;
  let split = 0;
  // Take items until we pass half the total; that index balances the two sides.
  for (let i = 0; i < items.length - 1; i++) {
    acc += items[i].size_bytes;
    split = i + 1;
    if (acc >= total / 2) break;
  }

  const first = items.slice(0, split);
  const second = items.slice(split);
  const ratio = acc / total;

  if (w >= h) {
    const cut = w * ratio;
    layout(first, x, y, cut, h, out);
    layout(second, x + cut, y, w - cut, h, out);
  } else {
    const cut = h * ratio;
    layout(first, x, y, w, cut, out);
    layout(second, x, y + cut, w, h - cut, out);
  }
}

export default function Treemap({
  entries,
  onOpen,
  height = 340,
}: {
  entries: DeepEntry[];
  onOpen: (entry: DeepEntry) => void;
  height?: number;
}) {
  const W = 1000;
  const H = height;

  const rects = useMemo(() => {
    const sized = entries.filter((e) => e.size_bytes > 0);
    if (sized.length === 0) return [];
    // Tiny slivers are unreadable and unclickable; fold the tail into the
    // largest items by simply dropping it from the map (the list still has it).
    const total = sized.reduce((a, e) => a + e.size_bytes, 0);
    const visible = sized.filter((e) => e.size_bytes / total >= 0.004).slice(0, 40);
    const out: Rect[] = [];
    layout(visible, 0, 0, W, H, out);
    return out;
  }, [entries, H]);

  if (rects.length === 0) return null;

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      width="100%"
      height={H}
      className="treemap"
      role="img"
      aria-label={`Treemap of ${rects.length} folders, sized by disk usage`}
    >
      {rects.map((r) => {
        // Labels only fit — and only mean anything — above a certain block size.
        const showName = r.w > 90 && r.h > 34;
        const showSize = r.w > 90 && r.h > 52;
        return (
          <g
            key={r.entry.path}
            className={`tm-cell ${r.entry.protected ? "is-protected" : ""}`}
            onClick={() => !r.entry.protected && r.entry.is_dir && onOpen(r.entry)}
          >
            <title>
              {r.entry.name} · {formatBytes(r.entry.size_bytes)}
              {r.entry.protected ? " · protected" : ""}
            </title>
            <rect
              x={r.x + 1}
              y={r.y + 1}
              width={Math.max(0, r.w - 2)}
              height={Math.max(0, r.h - 2)}
              rx="3"
              className={`tm-fill kindfill-${r.entry.kind}`}
            />
            {showName && (
              <text x={r.x + 10} y={r.y + 22} className="tm-name">
                {r.entry.name}
              </text>
            )}
            {showSize && (
              <text x={r.x + 10} y={r.y + 40} className="tm-size">
                {formatBytes(r.entry.size_bytes)}
              </text>
            )}
          </g>
        );
      })}
    </svg>
  );
}
