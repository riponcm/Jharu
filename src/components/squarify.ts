/// Squarified treemap layout (Bruls, Huizing & van Wijk, 2000), written out
/// here so the project needs no charting dependency. Pure functions only: no
/// DOM, no React, so it can be checked on its own (see squarify.check.ts).

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Weighted<T> {
  value: number;
  item: T;
}

export interface Placed<T> extends Box {
  item: T;
  value: number;
}

/// Worst aspect ratio (>= 1, lower is squarer) if `areas` were laid out as one
/// row along a side of length `side`.
function worst(areas: number[], side: number): number {
  let sum = 0;
  let max = -Infinity;
  let min = Infinity;
  for (const a of areas) {
    sum += a;
    if (a > max) max = a;
    if (a < min) min = a;
  }
  const s2 = side * side;
  return Math.max((s2 * max) / (sum * sum), (sum * sum) / (s2 * min));
}

/// Lays `items` out inside `box` so each rectangle's area is proportional to
/// its value and the rectangles stay as close to square as the data allows.
/// Items with a non-positive or non-finite value are left out. The result is
/// ordered largest first.
export function squarify<T>(items: Weighted<T>[], box: Box): Placed<T>[] {
  if (box.w <= 0 || box.h <= 0) return [];
  const sorted = items
    .filter((i) => Number.isFinite(i.value) && i.value > 0)
    .sort((a, b) => b.value - a.value);
  if (sorted.length === 0) return [];

  const total = sorted.reduce((a, i) => a + i.value, 0);
  const scale = (box.w * box.h) / total;
  const areas = sorted.map((i) => i.value * scale);

  const out: Placed<T>[] = [];
  let { x, y, w, h } = box;
  let start = 0;

  while (start < sorted.length) {
    // Rows run along the shorter side of what is left.
    const side = Math.min(w, h);
    let end = start + 1;
    while (
      end < sorted.length &&
      worst(areas.slice(start, end + 1), side) <= worst(areas.slice(start, end), side)
    ) {
      end++;
    }

    const rowAreas = areas.slice(start, end);
    const rowSum = rowAreas.reduce((a, b) => a + b, 0);
    const last = end >= sorted.length;

    if (w >= h) {
      // Column down the left edge.
      const colW = last ? w : rowSum / h;
      let cy = y;
      for (let i = start; i < end; i++) {
        const ch = i === end - 1 ? y + h - cy : areas[i] / colW;
        out.push({ x, y: cy, w: colW, h: ch, item: sorted[i].item, value: sorted[i].value });
        cy += ch;
      }
      x += colW;
      w -= colW;
    } else {
      // Row along the top edge.
      const rowH = last ? h : rowSum / w;
      let cx = x;
      for (let i = start; i < end; i++) {
        const cw = i === end - 1 ? x + w - cx : areas[i] / rowH;
        out.push({ x: cx, y, w: cw, h: rowH, item: sorted[i].item, value: sorted[i].value });
        cx += cw;
      }
      y += rowH;
      h -= rowH;
    }
    start = end;
  }
  return out;
}
