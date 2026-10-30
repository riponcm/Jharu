import { useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { formatBytes } from "../format";
import type { DeepEntry } from "../types";
import { deepVerdict, VERDICT_LABEL, type Verdict } from "./deepVerdict";
import { squarify, type Placed } from "./squarify";
import "../styles/treemap.css";

/// A box on the map: either a real entry, or a summary standing for entries too
/// small to draw one by one (hatched, never clickable).
interface Cell {
  key: string;
  name: string;
  size: number;
  entry: DeepEntry | null;
  level: Verdict | null;
  reasons: string[];
}

/// More boxes than this, or boxes under this share of the level, are unreadable
/// and unclickable; they are folded into one hatched summary box instead.
const MAX_BOXES = 60;
const MIN_SHARE = 0.004;

const TIP_W = 260;
// Generous: the tooltip grows with its reasons, and a clipped one hides the rating.
const TIP_H = 190;

function buildCells(entries: DeepEntry[]): { cells: Cell[]; unmeasured: number; omitted: number } {
  const sized = entries.filter((e) => e.size_bytes > 0).sort((a, b) => b.size_bytes - a.size_bytes);
  const total = sized.reduce((a, e) => a + e.size_bytes, 0);

  const shown = sized.filter((e, i) => i < MAX_BOXES && e.size_bytes / total >= MIN_SHARE);
  const rest = sized.slice(shown.length);

  const cells: Cell[] = shown.map((entry) => {
    const verdict = deepVerdict(entry);
    return {
      key: entry.path,
      name: entry.name,
      size: entry.size_bytes,
      entry,
      level: verdict.level,
      reasons: verdict.reasons,
    };
  });

  // The summary box only earns its place if the leftovers add up to something
  // visible; otherwise it would be a sliver, and they are counted in the footnote.
  const restSize = rest.reduce((a, e) => a + e.size_bytes, 0);
  let omitted = 0;
  if (rest.length > 0 && restSize / total >= MIN_SHARE) {
    cells.push({
      key: "__smaller__",
      name: `${rest.length} smaller item${rest.length === 1 ? "" : "s"}`,
      size: restSize,
      entry: null,
      level: null,
      reasons: ["Too small to draw as separate boxes.", "Switch to List to see each one."],
    });
  } else {
    omitted = rest.length;
  }

  return { cells, unmeasured: entries.length - sized.length, omitted };
}

export default function Treemap({
  entries,
  onOpen,
  height = 380,
}: {
  entries: DeepEntry[];
  onOpen: (entry: DeepEntry) => void;
  height?: number;
}) {
  const frameRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(900);
  const [active, setActive] = useState<string | null>(null);

  useLayoutEffect(() => {
    const el = frameRef.current;
    if (!el) return;
    const update = () => setWidth(Math.max(200, el.clientWidth));
    update();
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const { cells, unmeasured, omitted } = useMemo(() => buildCells(entries), [entries]);

  const placed = useMemo<Placed<Cell>[]>(
    () =>
      squarify(
        cells.map((c) => ({ value: c.size, item: c })),
        { x: 0, y: 0, w: width, h: height }
      ),
    [cells, width, height]
  );

  const total = cells.reduce((a, c) => a + c.size, 0);
  const activeBox = placed.find((p) => p.item.key === active) ?? null;
  const levelsShown = [...new Set(cells.map((c) => c.level).filter((l): l is Verdict => !!l))];

  // Arrow keys step through the boxes largest to smallest, so the map is
  // usable without a mouse (Tab also visits each box; Enter opens it).
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step =
      e.key === "ArrowRight" || e.key === "ArrowDown" ? 1
      : e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1
      : 0;
    if (!step) return;
    const boxes = Array.from(e.currentTarget.querySelectorAll<HTMLButtonElement>(".tm-box"));
    const at = boxes.indexOf(document.activeElement as HTMLButtonElement);
    if (at < 0) return;
    e.preventDefault();
    boxes[Math.min(boxes.length - 1, Math.max(0, at + step))]?.focus();
  };

  // Tooltip sits under the hovered box when there is room, otherwise above it,
  // and failing both (a box taller than the space around it) over the box
  // itself. It is always kept inside the frame.
  const tipStyle = activeBox && {
    left: Math.max(4, Math.min(width - TIP_W - 4, activeBox.x)),
    ...(activeBox.y + activeBox.h + TIP_H + 6 <= height
      ? { top: activeBox.y + activeBox.h + 6 }
      : activeBox.y >= TIP_H + 6
        ? { bottom: height - activeBox.y + 6 }
        : { top: Math.max(4, Math.min(height - TIP_H - 4, activeBox.y + 6)) }),
  };

  return (
    <div className="tm-scope">
      <div
        ref={frameRef}
        className="tm"
        style={{ height }}
        role="group"
        aria-label={`Disk map: ${placed.length} boxes sized by disk usage and coloured by safety`}
        onKeyDown={onKeyDown}
      >
        {placed.length === 0 && <div className="tm-empty">Nothing measurable at this level yet</div>}

        {placed.map((p) => {
          const { item: c } = p;
          const openable = !!c.entry && c.entry.is_dir;
          const share = total > 0 ? (c.size / total) * 100 : 0;
          const showName = p.w >= 64 && p.h >= 30;
          const showSize = showName && p.h >= 48;
          const showTag = !!c.level && p.w >= 104 && p.h >= 68;
          const summary =
            `${c.name}, ${formatBytes(c.size)}, ${share.toFixed(1)}% of this level` +
            (c.level ? `, ${VERDICT_LABEL[c.level]}` : "") +
            `. ${c.reasons.join(" ")}` +
            (openable ? " Press Enter to open." : "");

          return (
            <button
              key={c.key}
              type="button"
              className={
                `tm-box ${c.level ? `v-${c.level}` : "is-virtual"}` +
                `${openable ? "" : " is-static"}${active === c.key ? " is-active" : ""}`
              }
              style={{ left: p.x, top: p.y, width: p.w, height: p.h }}
              aria-label={summary}
              aria-disabled={!openable}
              onClick={() => openable && onOpen(c.entry!)}
              onMouseEnter={() => setActive(c.key)}
              onMouseLeave={() => setActive((a) => (a === c.key ? null : a))}
              onFocus={() => setActive(c.key)}
              onBlur={() => setActive((a) => (a === c.key ? null : a))}
            >
              {showName && <span className="tm-name">{c.name}</span>}
              {showSize && <span className="tm-meta">{formatBytes(c.size)}</span>}
              {showTag && <span className="tm-tag">{VERDICT_LABEL[c.level!]}</span>}
            </button>
          );
        })}

        {activeBox && tipStyle && (
          <div className="tm-tip" role="tooltip" style={tipStyle}>
            <div className="tm-tip-head">
              <span>{activeBox.item.name}</span>
              <span className="tm-tip-size">{formatBytes(activeBox.item.size)}</span>
            </div>
            <div className="tm-tip-verdict">
              {activeBox.item.level ? VERDICT_LABEL[activeBox.item.level] : "Not shown one by one"}
              {total > 0 && ` · ${((activeBox.item.size / total) * 100).toFixed(1)}% of this level`}
            </div>
            <ul>
              {activeBox.item.reasons.map((r) => (
                <li key={r}>{r}</li>
              ))}
            </ul>
            {activeBox.item.entry?.is_dir && <div className="tm-tip-action">Click or press Enter to open</div>}
          </div>
        )}
      </div>

      {(levelsShown.length > 0 || cells.some((c) => !c.level)) && (
        <div className="tm-legend">
          {levelsShown.map((l) => (
            <span key={l} className="tm-legend-item">
              <i className={`tm-swatch v-${l}`} />
              {VERDICT_LABEL[l]}
            </span>
          ))}
          {cells.some((c) => !c.level) && (
            <span className="tm-legend-item">
              <i className="tm-swatch is-virtual" />
              Too small to show
            </span>
          )}
        </div>
      )}

      {unmeasured + omitted > 0 && (
        <p className="footnote">
          {[
            unmeasured > 0 && `${unmeasured} measured 0 B (empty, or not readable)`,
            omitted > 0 && `${omitted} too small to see`,
          ]
            .filter(Boolean)
            .join(" and ")}
          {" "}— not drawn. List shows every item.
        </p>
      )}
    </div>
  );
}
