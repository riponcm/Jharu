import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatBytes, isWindows } from "../format";
import type { Reason, Safety } from "../types";
import GadgetSettings from "../components/GadgetSettings";
import "../styles/speed.css";

interface Member {
  pid: number;
  start: number;
}

interface ProcessGroup {
  id: string;
  name: string;
  file: string;
  publisher: string | null;
  exe: string | null;
  members: Member[];
  process_count: number;
  child_count: number;
  cpu_percent: number;
  memory_bytes: number;
  disk_read_per_sec: number;
  disk_write_per_sec: number;
  has_window: boolean | null;
  window_count: number;
  not_responding: boolean;
  owner: "you" | "other" | "unknown";
  runs_as_you: boolean;
  started: number;
  safety: Safety;
  summary: string;
  reasons: Reason[];
  offered: boolean;
}

interface Totals {
  cpu_percent: number;
  memory_used: number;
  memory_total: number;
  swap_used: number;
  swap_total: number;
  disk_read_per_sec: number;
  disk_write_per_sec: number;
  process_count: number;
}

interface ProcessReport {
  supported: boolean;
  windows_known: boolean;
  totals: Totals;
  groups: ProcessGroup[];
  notes: string[];
}

type Status = "exited" | "still_running" | "no_window" | "skipped" | "failed";

interface ItemResult {
  id: string;
  name: string;
  status: Status;
  message: string;
  exited: number[];
  running: number[];
}

interface CloseOutcome {
  results: ItemResult[];
  cancelled: boolean;
}

type Sort = "cpu" | "memory" | "disk";
type Filter = "all" | "heavy" | "window" | "background";

/// How often the numbers refresh while this tab is showing.
const POLL_MS = 2000;
/// Row order is only rearranged this often, and never while the mouse is over
/// the list, so a row does not move as it is being clicked.
const REORDER_MS = 10000;
/// What counts as heavy for the "Heavy" filter.
const HEAVY_CPU = 5;
const HEAVY_MEMORY = 500 * 1024 * 1024;
const HEAVY_DISK = 5 * 1024 * 1024;

const diskOf = (g: ProcessGroup) => g.disk_read_per_sec + g.disk_write_per_sec;
const isHeavy = (g: ProcessGroup) =>
  g.cpu_percent >= HEAVY_CPU || g.memory_bytes >= HEAVY_MEMORY || diskOf(g) >= HEAVY_DISK;

const SORTERS: Record<Sort, (g: ProcessGroup) => number> = {
  cpu: (g) => g.cpu_percent,
  memory: (g) => g.memory_bytes,
  disk: diskOf,
};

const pct = (n: number) => `${n < 10 ? n.toFixed(1) : Math.round(n)}%`;
const perSec = (n: number) => `${formatBytes(n)}/s`;

function runningFor(startUnix: number): string {
  const s = Math.max(0, Date.now() / 1000 - startUnix);
  if (s < 90) return "just started";
  if (s < 3600) return `${Math.round(s / 60)} min`;
  if (s < 86400) return `${(s / 3600).toFixed(1)} h`;
  return `${Math.round(s / 86400)} d`;
}

function Meter({ value, label }: { value: number; label: string }) {
  const v = Math.min(100, Math.max(0, value));
  return (
    <div className="sp-meter" role="img" aria-label={`${label} ${Math.round(v)}%`}>
      <div className={`sp-meter-fill ${v >= 90 ? "is-hot" : v >= 75 ? "is-warm" : ""}`} style={{ width: `${v}%` }} />
    </div>
  );
}

function Summary({ t }: { t: Totals }) {
  const memPct = t.memory_total ? (t.memory_used / t.memory_total) * 100 : 0;
  const swapPct = t.swap_total ? (t.swap_used / t.swap_total) * 100 : 0;
  return (
    <section className="sp-summary" aria-label="Live load">
      <div className="sp-tile">
        <span className="metric-label">CPU</span>
        <span className="sp-value">{pct(t.cpu_percent)}</span>
        <Meter value={t.cpu_percent} label="CPU" />
      </div>
      <div className="sp-tile">
        <span className="metric-label">Memory</span>
        <span className="sp-value">
          {formatBytes(t.memory_used)} <span className="metric-sub">of {formatBytes(t.memory_total)}</span>
        </span>
        <Meter value={memPct} label="Memory" />
      </div>
      <div className="sp-tile">
        <span className="metric-label">Swap / page file</span>
        <span className="sp-value">
          {formatBytes(t.swap_used)} <span className="metric-sub">of {formatBytes(t.swap_total)}</span>
        </span>
        <Meter value={swapPct} label="Swap" />
      </div>
      <div className="sp-tile">
        <span className="metric-label">Disk activity</span>
        <span className="sp-value sp-value-sm">
          <span title="Read">↓ {perSec(t.disk_read_per_sec)}</span>
          <span title="Written">↑ {perSec(t.disk_write_per_sec)}</span>
        </span>
        <span className="metric-sub">all programs' reads and writes</span>
      </div>
      <div className="sp-tile">
        <span className="metric-label">Processes</span>
        <span className="sp-value">{t.process_count}</span>
      </div>
    </section>
  );
}

export default function Speed() {
  const [report, setReport] = useState<ProcessReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sort, setSort] = useState<Sort>("cpu");
  const [filter, setFilter] = useState<Filter>("all");
  const [order, setOrder] = useState<string[]>([]);
  const [closing, setClosing] = useState<{ id: string; token: string } | null>(null);
  const [forcing, setForcing] = useState<string | null>(null);
  const [confirmForce, setConfirmForce] = useState<ProcessGroup | null>(null);
  const [results, setResults] = useState<Record<string, ItemResult>>({});
  const [notices, setNotices] = useState<string[]>([]);
  const [open, setOpen] = useState<Set<string>>(new Set());

  const root = useRef<HTMLDivElement>(null);
  const hovering = useRef(false);
  const inflight = useRef(false);
  const lastPoll = useRef(0);
  const lastOrder = useRef(0);
  // Read inside the poller, which must not be re-created as these change.
  const view = useRef({ sort, filter, confirm: false });
  view.current = { sort, filter, confirm: confirmForce != null };

  const arrange = useCallback((r: ProcessReport, force: boolean) => {
    const { sort, filter } = view.current;
    const now = Date.now();
    const settled = !hovering.current && !view.current.confirm;
    if (!force && !(settled && now - lastOrder.current >= REORDER_MS)) return;
    lastOrder.current = now;
    const shownIn = r.groups.filter((g) => {
      switch (filter) {
        case "heavy":
          return isHeavy(g);
        case "window":
          return g.has_window === true;
        case "background":
          return g.has_window === false;
        default:
          return true;
      }
    });
    setOrder(shownIn.sort((a, b) => SORTERS[sort](b) - SORTERS[sort](a)).map((g) => g.id));
  }, []);

  const poll = useCallback(async () => {
    if (inflight.current) return;
    inflight.current = true;
    try {
      const r = await invoke<ProcessReport>("process_list");
      setReport(r);
      setError(null);
      arrange(r, lastOrder.current === 0);
    } catch (e) {
      setError(String(e));
    } finally {
      inflight.current = false;
      lastPoll.current = Date.now();
    }
  }, [arrange]);

  // Refreshes every two seconds, but only while this tab is showing: the view
  // stays mounted behind other tabs, where its numbers would be wasted work.
  useEffect(() => {
    const tick = () => {
      const showing = !document.hidden && root.current != null && root.current.offsetParent !== null;
      if (showing && Date.now() - lastPoll.current >= POLL_MS) poll();
    };
    tick();
    const t = window.setInterval(tick, 500);
    return () => window.clearInterval(t);
  }, [poll]);

  // A new sort or filter rearranges at once.
  useEffect(() => {
    if (report) arrange(report, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sort, filter]);

  const byId = useMemo(() => new Map((report?.groups ?? []).map((g) => [g.id, g])), [report]);

  // Rows in the frozen order; programs that appeared since are added at the
  // end until the next rearrangement.
  const shown = useMemo(() => {
    const listed = order.map((id) => byId.get(id)).filter((g): g is ProcessGroup => g != null);
    return listed;
  }, [order, byId]);

  // A program that has ended no longer needs its message under the row.
  useEffect(() => {
    if (!report) return;
    setResults((prev) => {
      const keep = Object.fromEntries(Object.entries(prev).filter(([id]) => byId.has(id)));
      return Object.keys(keep).length === Object.keys(prev).length ? prev : keep;
    });
  }, [report, byId]);

  const target = (g: ProcessGroup) => ({ id: g.id, name: g.name, members: g.members });

  const noteResult = (r: ItemResult) => {
    if (r.status === "exited") {
      setNotices((n) => [r.message, ...n].slice(0, 4));
      setResults((prev) => {
        const next = { ...prev };
        delete next[r.id];
        return next;
      });
    } else {
      setResults((prev) => ({ ...prev, [r.id]: r }));
    }
  };

  const close = async (g: ProcessGroup) => {
    const token = `sp:${Math.random()}`;
    setClosing({ id: g.id, token });
    setError(null);
    try {
      const out = await invoke<CloseOutcome>("process_close", { items: [target(g)], token });
      out.results.forEach(noteResult);
    } catch (e) {
      setError(String(e));
    } finally {
      setClosing(null);
      poll();
    }
  };

  const forceQuit = async (g: ProcessGroup) => {
    setConfirmForce(null);
    setForcing(g.id);
    setError(null);
    try {
      const out = await invoke<CloseOutcome>("process_force_quit", { items: [target(g)] });
      out.results.forEach(noteResult);
    } catch (e) {
      setError(String(e));
    } finally {
      setForcing(null);
      poll();
    }
  };

  const toggleWhy = (id: string) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const groups = report?.groups ?? [];
  const counts = {
    all: groups.length,
    heavy: groups.filter(isHeavy).length,
    window: groups.filter((g) => g.has_window === true).length,
    background: groups.filter((g) => g.has_window === false).length,
  };
  const busy = closing != null || forcing != null;

  return (
    <div ref={root}>
      {confirmForce && (
        <div className="modal-backdrop" role="dialog" aria-modal="true" onClick={() => setConfirmForce(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3 className="modal-title">Force quit {confirmForce.name}?</h3>
            <p className="modal-body">
              Force quit ends the program immediately and does not ask it first. Anything in it that you have not saved is
              lost{confirmForce.process_count > 1 ? `, in all ${confirmForce.process_count} of its processes` : ""}, and
              programs it started may stop too. Use it only when closing normally did not work.
            </p>
            {confirmForce.has_window === true && (
              <p className="modal-warn">It has a window open, so it may hold work you have not saved.</p>
            )}
            <div className="modal-items">
              <code className="modal-item">{confirmForce.file}</code>
            </div>
            <div className="modal-actions">
              <button className="btn" onClick={() => setConfirmForce(null)}>
                No, leave it
              </button>
              <button className="btn-danger" onClick={() => forceQuit(confirmForce)}>
                Yes, force quit
              </button>
            </div>
          </div>
        </div>
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Speed</h2>
          <p className="view-sub">
            What is using your computer right now. Close a program that is slowing things down, one at a time
          </p>
        </div>
        <button className="btn" onClick={poll}>
          Refresh now
        </button>
      </div>

      <GadgetSettings />

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {notices.map((n, i) => (
        <div key={`${i}:${n}`} className="banner-info">
          {n}
          <button className="btn-ghost sm sp-dismiss" onClick={() => setNotices((all) => all.filter((_, j) => j !== i))}>
            Dismiss
          </button>
        </div>
      ))}

      {!report && !error && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Reading running programs…</p>
        </div>
      )}

      {report && !report.supported && (
        <div className="banner-info">The task killer only lists programs on Windows and macOS.</div>
      )}

      {report && report.supported && (
        <>
          <Summary t={report.totals} />

          <div className="banner-info">
            Jharu never closes programs in bulk or on its own, and never offers system or security programs. Close asks the
            program first, the same as the X on its window, so it can ask you to save. Force quit is a separate, second step
            that loses anything unsaved.
          </div>
          {report.notes.map((n) => (
            <div key={n} className="banner-info">{n}</div>
          ))}

          <div className="filter-row">
            <div className="filter-tabs" role="tablist" aria-label="Show">
              {(
                [
                  ["all", `All (${counts.all})`],
                  ["heavy", `Heavy (${counts.heavy})`],
                  ["window", `Has window (${counts.window})`],
                  ["background", `Background (${counts.background})`],
                ] as [Filter, string][]
              ).map(([id, label]) => (
                <button
                  key={id}
                  className={`filter-tab ${filter === id ? "is-active" : ""}`}
                  onClick={() => setFilter(id)}
                >
                  {label}
                </button>
              ))}
            </div>
            <div className="filter-tabs" role="tablist" aria-label="Sort by">
              {(
                [
                  ["cpu", "CPU"],
                  ["memory", "Memory"],
                  ["disk", "Disk"],
                ] as [Sort, string][]
              ).map(([id, label]) => (
                <button
                  key={id}
                  className={`filter-tab ${sort === id ? "is-active" : ""}`}
                  onClick={() => setSort(id)}
                >
                  {id === sort ? `${label} ↓` : label}
                </button>
              ))}
            </div>
          </div>

          {shown.length === 0 && (
            <div className="empty-state">
              <p>No programs match this filter.</p>
            </div>
          )}

          {shown.length > 0 && (
            <div
              className="findings sp-list"
              onMouseEnter={() => (hovering.current = true)}
              onMouseLeave={() => (hovering.current = false)}
              onFocus={() => (hovering.current = true)}
              onBlur={() => (hovering.current = false)}
            >
              {shown.map((g) => {
                const result = results[g.id];
                const attempted = result != null && result.status !== "skipped";
                const closingThis = closing?.id === g.id;
                const forcingThis = forcing === g.id;
                const risky = !g.offered;
                const why = open.has(g.id) || risky;
                const inline = g.reasons.find((r) => r.level !== "ok");
                return (
                  <div key={g.id} className={`model-row sp-row ${risky ? "is-protected" : ""}`}>
                    <div className="model-main">
                      <div className="finding-title">
                        <span className="finding-name">{g.name}</span>
                        <span className={`badge safety-${g.safety}`}>{g.summary}</span>
                        {g.process_count > 1 && <span className="badge">{g.process_count} processes</span>}
                        {g.not_responding && <span className="badge safety-risky">Not responding</span>}
                        {g.has_window === true && <span className="badge cat-dev_cache">Window</span>}
                        {g.child_count > 0 && (
                          <span className="badge">
                            {g.child_count} started from it
                          </span>
                        )}
                      </div>
                      <span className="sp-sub">
                        {g.file}
                        {g.publisher && ` · ${g.publisher}`} · running {runningFor(g.started)}
                      </span>
                      <div className="sp-nums">
                        <span title="Share of the whole computer's processor">
                          <b>{pct(g.cpu_percent)}</b> CPU
                        </span>
                        <span>
                          <b>{formatBytes(g.memory_bytes)}</b> memory
                        </span>
                        <span title="Read and written since the last refresh, per second">
                          <b>{perSec(diskOf(g))}</b> disk
                        </span>
                      </div>
                      {!why && inline && <p className="sp-inline">{inline.text}</p>}
                      {why && (
                        <ul className="safety-reasons">
                          {g.reasons.map((r) => (
                            <li key={r.text} className={`reason-${r.level}`}>
                              <span className="reason-mark" aria-hidden>
                                {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
                              </span>
                              {r.text}
                            </li>
                          ))}
                        </ul>
                      )}
                      {result && <p className={`sp-result is-${result.status}`}>{result.message}</p>}
                      {!risky && (
                        <button className="sp-why" onClick={() => toggleWhy(g.id)}>
                          {open.has(g.id) ? "Hide the reasons" : "Why this rating?"}
                        </button>
                      )}
                    </div>
                    <div className="sp-actions">
                      {risky ? (
                        <span className="sp-note">Protected: Jharu will not close this</span>
                      ) : (
                        <>
                          <button className="btn sm" disabled={busy} onClick={() => close(g)}>
                            {closingThis ? "Waiting…" : "Close"}
                          </button>
                          {closingThis && closing && (
                            <button
                              className="btn-ghost sm"
                              onClick={() => invoke("process_cancel", { token: closing.token }).catch(() => {})}
                            >
                              Stop waiting
                            </button>
                          )}
                          {attempted && !closingThis && (
                            <button className="btn-danger sm" disabled={busy} onClick={() => setConfirmForce(g)}>
                              {forcingThis ? "Ending…" : "Force quit"}
                            </button>
                          )}
                        </>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          )}

          <p className="footnote">
            Numbers refresh every two seconds while this tab is open, and the order only changes every ten seconds or when
            you pick a sort or filter, so rows stay still under your mouse. CPU is the share of the whole computer. Disk is
            what each program reads and writes, which includes network and device traffic on Windows.{" "}
            {isWindows
              ? "Rows are grouped by program and user. Programs are rated Safe only when Jharu can see they have no window, nothing started from them, and nothing else running started them."
              : "Rows are grouped by app, with its helper processes. Programs are rated Safe only when Jharu can see they have no window, nothing started from them, and nothing else running started them. Close asks an app to quit the way the Dock does, so it can offer to save; other programs get a quit request (SIGTERM)."}
          </p>
        </>
      )}
    </div>
  );
}
