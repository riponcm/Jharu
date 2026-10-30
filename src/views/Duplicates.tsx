import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import type { Reason, Safety } from "../types";
import "../styles/dupes.css";

type Kind = "files" | "model" | "package";
type Filter = "all" | "tools" | "files";

interface RootInfo {
  path: string;
  label: string;
}

interface DupeCopy {
  path: string;
  link_paths: string[];
  size_bytes: number;
  modified_days: number | null;
  place: string;
  safety: Safety;
  reasons: Reason[];
  removable: boolean;
  reclaim_bytes: number;
  preselect: boolean;
  keep: boolean;
  keep_note: string | null;
}

interface DupeGroup {
  hash: string;
  name: string;
  kind: Kind;
  size_bytes: number;
  copies: DupeCopy[];
  wasted_bytes: number;
}

interface HardLinkSet {
  name: string;
  size_bytes: number;
  paths: string[];
  shared_bytes: number;
  /** APFS clones (macOS) rather than hard links. */
  clones: boolean;
}

interface DupeReport {
  groups: DupeGroup[];
  hard_links: HardLinkSet[];
  hard_link_bytes: number;
  wasted_bytes: number;
  safe_bytes: number;
  files_seen: number;
  online_only_skipped: number;
  unreadable_count: number;
  unreadable: string[];
  min_bytes: number;
  roots: string[];
  caches: string[];
  cancelled: boolean;
  seconds: number;
}

interface RemoveOutcome {
  freed_bytes: number;
  removed: string[];
  skipped: string[];
  errors: string[];
  cancelled: boolean;
}

interface Progress {
  phase: string;
  done: number;
  total: number;
  current: string;
}

const BIN = isWindows ? "Recycle Bin" : "Trash";
const PAGE = 60;

const MIN_SIZES: { label: string; bytes: number }[] = [
  { label: "64 KB", bytes: 64 << 10 },
  { label: "256 KB", bytes: 256 << 10 },
  { label: "1 MB", bytes: 1 << 20 },
  { label: "10 MB", bytes: 10 << 20 },
  { label: "100 MB", bytes: 100 << 20 },
];

const SAFETY_LABEL: Record<Safety, string> = { safe: "Safe", caution: "Caution", risky: "Risky" };
const KIND_LABEL: Record<Kind, string> = { files: "Your files", model: "ML model", package: "Package" };

function ageLabel(days: number | null): string {
  if (days == null) return "unknown age";
  if (days < 1) return "changed today";
  if (days < 30) return `changed ${days}d ago`;
  if (days < 365) return `changed ${Math.round(days / 30)} mo ago`;
  return `changed ${(days / 365).toFixed(1)} yr ago`;
}

function phaseText(p: Progress): string {
  switch (p.phase) {
    case "listing":
      return `Listing files · ${p.done.toLocaleString()} found`;
    case "checking":
      return `Checking which files are the same file · ${p.done} of ${p.total}`;
    case "comparing":
      return `Comparing the start and end of ${p.total} files · ${p.done} done`;
    case "hashing":
      return `Reading whole files · ${formatBytes(p.done)} of ${formatBytes(p.total)}`;
    case "removing":
      return `Moving copies to the ${BIN} · ${Math.min(p.done, p.total)} of ${p.total}`;
    default:
      return "Working…";
  }
}

function progressFraction(p: Progress): number | null {
  if (p.phase === "listing" || p.total === 0) return null;
  return Math.min(1, p.done / p.total);
}

export default function Duplicates({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [roots, setRoots] = useState<RootInfo[]>([]);
  const [adding, setAdding] = useState("");
  const [minBytes, setMinBytes] = useState(1 << 20);
  const [includeCaches, setIncludeCaches] = useState(true);

  const [report, setReport] = useState<DupeReport | null>(null);
  const [scanning, setScanning] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<RemoveOutcome | null>(null);

  const [filter, setFilter] = useState<Filter>("all");
  const [shown, setShown] = useState(PAGE);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [keepOverride, setKeepOverride] = useState<Record<string, string>>({});
  const [confirming, setConfirming] = useState(false);
  const activeToken = useRef<string | null>(null);

  const busy = scanning || removing;

  useEffect(() => {
    invoke<RootInfo[]>("dupes_default_roots").then(setRoots).catch((e) => setError(String(e)));
    let unlisten: UnlistenFn | undefined;
    listen<Progress & { token: string }>("dupes-progress", (event) => {
      if (event.payload.token !== activeToken.current) return;
      setProgress(event.payload);
    }).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  const addFolder = async () => {
    const text = adding.trim();
    if (!text) return;
    try {
      const info = await invoke<RootInfo>("dupes_check_folder", { path: text });
      setRoots((prev) =>
        prev.some((r) => r.path.toLowerCase() === info.path.toLowerCase()) ? prev : [...prev, info]
      );
      setAdding("");
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const keeperOf = (g: DupeGroup): DupeCopy =>
    g.copies.find((c) => c.path === keepOverride[g.hash]) ?? g.copies.find((c) => c.keep) ?? g.copies[0];

  const scan = async () => {
    const token = `dupes:${Math.random()}`;
    activeToken.current = token;
    setScanning(true);
    setError(null);
    setOutcome(null);
    setProgress({ phase: "listing", done: 0, total: 0, current: "" });
    try {
      const r = await invoke<DupeReport>("dupes_scan", {
        request: { roots: roots.map((x) => x.path), min_bytes: minBytes, include_caches: includeCaches },
        token,
      });
      setReport(r);
      setShown(PAGE);
      setKeepOverride({});
      setSelected(new Set(r.groups.flatMap((g) => g.copies.filter((c) => c.preselect).map((c) => c.path))));
    } catch (e) {
      setError(String(e));
    } finally {
      activeToken.current = null;
      setScanning(false);
      setProgress(null);
    }
  };

  const cancel = () => {
    if (activeToken.current) invoke("dupes_cancel", { token: activeToken.current }).catch(() => {});
  };

  const toggle = (c: DupeCopy) => {
    if (!c.removable) return;
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(c.path)) next.delete(c.path);
      else next.add(c.path);
      return next;
    });
  };

  const chooseKeeper = (g: DupeGroup, c: DupeCopy) => {
    setKeepOverride((prev) => ({ ...prev, [g.hash]: c.path }));
    // The copy that is kept can never also be removed.
    setSelected((prev) => {
      if (!prev.has(c.path)) return prev;
      const next = new Set(prev);
      next.delete(c.path);
      return next;
    });
  };

  const selectGroup = (g: DupeGroup) => {
    const keeper = keeperOf(g);
    setSelected((prev) => {
      const next = new Set(prev);
      g.copies.filter((c) => c.path !== keeper.path && c.removable).forEach((c) => next.add(c.path));
      return next;
    });
  };

  const groups = report?.groups ?? [];
  const visible = useMemo(
    () =>
      groups.filter((g) =>
        filter === "all" ? true : filter === "tools" ? g.kind !== "files" : g.kind === "files"
      ),
    [groups, filter]
  );

  // What a removal would do: per group, the ticked copies other than the one kept.
  const plan = useMemo(() => {
    return groups
      .map((g) => {
        const keeper = g.copies.find((c) => c.path === keepOverride[g.hash]) ?? g.copies.find((c) => c.keep) ?? g.copies[0];
        const remove = g.copies.filter((c) => c.path !== keeper.path && c.removable && selected.has(c.path));
        return { g, keeper, remove };
      })
      .filter((x) => x.remove.length > 0);
  }, [groups, keepOverride, selected]);

  const planCopies = plan.flatMap((x) => x.remove);
  const planBytes = planCopies.reduce((a, c) => a + c.reclaim_bytes, 0);
  const planRisky = planCopies.filter((c) => c.safety === "risky").length;
  const planCaution = planCopies.filter((c) => c.safety === "caution").length;

  const selectAllSafe = () =>
    setSelected(
      new Set(
        groups.flatMap((g) => {
          const keeper = keeperOf(g);
          return g.copies
            .filter((c) => c.path !== keeper.path && c.removable && c.safety === "safe")
            .map((c) => c.path);
        })
      )
    );

  const removeSelected = async () => {
    setConfirming(false);
    setOutcome(null);
    const token = `dupes-rm:${Math.random()}`;
    activeToken.current = token;
    setRemoving(true);
    setProgress({ phase: "removing", done: 0, total: planCopies.length, current: "" });
    try {
      const result = await invoke<RemoveOutcome>("dupes_remove", {
        groups: plan.map(({ g, keeper, remove }) => ({
          hash: g.hash,
          size_bytes: g.size_bytes,
          keep: [keeper.path, ...keeper.link_paths],
          remove: remove.map((c) => ({ paths: [c.path, ...c.link_paths], expected: c.safety })),
        })),
        token,
      });
      setOutcome(result);
      if (result.errors.length) setError(result.errors.join("; "));
      onRecovered(result.freed_bytes);
      // Take what was moved out of the report instead of rescanning for minutes.
      const gone = new Set(result.removed);
      setReport((prev) => {
        if (!prev) return prev;
        const next = prev.groups
          .map((g) => ({ ...g, copies: g.copies.filter((c) => !gone.has(c.path)) }))
          .filter((g) => g.copies.length >= 2)
          .map((g) => {
            const kept = g.copies.find((c) => c.keep) ?? g.copies[0];
            return { ...g, wasted_bytes: g.copies.filter((c) => c !== kept && c.removable).reduce((a, c) => a + c.reclaim_bytes, 0) };
          });
        const safe = next.reduce(
          (a, g) => a + g.copies.filter((c) => !c.keep && c.removable && c.safety === "safe").reduce((b, c) => b + c.reclaim_bytes, 0),
          0
        );
        return { ...prev, groups: next, wasted_bytes: next.reduce((a, g) => a + g.wasted_bytes, 0), safe_bytes: safe };
      });
      setSelected(new Set());
    } catch (e) {
      setError(String(e));
    } finally {
      activeToken.current = null;
      setRemoving(false);
      setProgress(null);
    }
  };

  const fraction = progress ? progressFraction(progress) : null;
  const toolGroups = groups.filter((g) => g.kind !== "files").length;

  return (
    <>
      {confirming && (
        <div className="modal-backdrop" role="dialog" aria-modal="true" onClick={() => setConfirming(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3 className="modal-title">
              Move {planCopies.length} duplicate cop{planCopies.length === 1 ? "y" : "ies"} to the {BIN}?
            </h3>
            <p className="modal-body">
              Every group keeps one copy. Just before moving anything, Jharu reads the copy you keep and each
              copy you remove again and compares them with what the scan found. A copy that changed, is in
              use, or is now rated worse is left where it is.
            </p>
            {(planCaution > 0 || planRisky > 0) && (
              <p className="modal-warn">
                {planCaution > 0 &&
                  `${planCaution} of these ${planCaution === 1 ? "is" : "are"} rated Caution: the content stays in the copy you keep, but a program may still point at the one you remove. `}
                {planRisky > 0 &&
                  `${planRisky} ${planRisky === 1 ? "is" : "are"} rated Risky (for example in a cloud-sync folder, where removing it also removes it from your other devices): check its reasons first.`}
              </p>
            )}
            <div className="modal-items">
              {planCopies.slice(0, 4).map((c) => (
                <code key={c.path} className="modal-item">{c.path}</code>
              ))}
              {planCopies.length > 4 && <span className="modal-more">and {planCopies.length - 4} more</span>}
            </div>
            <div className="modal-total">
              <span>Space reclaimed</span>
              <strong>{formatBytes(planBytes)}</strong>
            </div>
            <p className="modal-foot">
              Everything moves to the {BIN}, so you can put it back until you empty it.
            </p>
            <div className="modal-actions">
              <button className="btn" onClick={() => setConfirming(false)}>No, keep them</button>
              <button className="btn-danger" onClick={removeSelected}>Yes, move to {BIN}</button>
            </div>
          </div>
        </div>
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Duplicates</h2>
          <p className="view-sub">
            Identical files in your folders, and the same model or package stored by several tools
          </p>
        </div>
        <button className="btn" onClick={scan} disabled={busy || roots.length + (includeCaches ? 1 : 0) === 0}>
          {scanning ? "Scanning…" : report ? "Scan again" : "Find duplicates"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      <section className="dupes-setup">
        <div>
          <h3>Folders to search</h3>
          <div className="dupes-roots" style={{ marginTop: 8 }}>
            {roots.length === 0 && <span className="model-meta">No folders yet — add one below.</span>}
            {roots.map((r) => (
              <span key={r.path} className="dupes-chip" title={r.path}>
                {r.label}
                <code>{r.path}</code>
                <button
                  aria-label={`Do not search ${r.label}`}
                  disabled={busy}
                  onClick={() => setRoots((prev) => prev.filter((x) => x.path !== r.path))}
                >
                  ×
                </button>
              </span>
            ))}
          </div>
        </div>
        <div className="dupes-add">
          <input
            type="text"
            value={adding}
            placeholder={isWindows ? "Add a folder, e.g. D:\\Photos" : "Add a folder, e.g. /Volumes/Archive"}
            onChange={(e) => setAdding(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && addFolder()}
            disabled={busy}
          />
          <button className="btn" onClick={addFolder} disabled={busy || !adding.trim()}>Add folder</button>
        </div>
        <div className="dupes-options">
          <label>
            Ignore files smaller than
            <select value={minBytes} onChange={(e) => setMinBytes(Number(e.target.value))} disabled={busy}>
              {MIN_SIZES.map((m) => (
                <option key={m.bytes} value={m.bytes}>{m.label}</option>
              ))}
            </select>
          </label>
          <label>
            <input
              type="checkbox"
              checked={includeCaches}
              onChange={(e) => setIncludeCaches(e.target.checked)}
              disabled={busy}
            />
            Also compare ML model and package caches (Hugging Face, Ollama, LM Studio, PyTorch, pip, uv, conda)
          </label>
        </div>
      </section>

      {progress && (
        <div className="banner-info dupes-progress">
          <div className="meter-head">
            <span>{phaseText(progress)}</span>
          </div>
          <div className="meter-track">
            <div
              className="meter-fill"
              style={{
                width: `${fraction == null ? 8 : Math.max(3, fraction * 100)}%`,
                background: "var(--accent)",
              }}
            />
          </div>
          <div className="dupes-progress-actions">
            <span className="model-meta">
              {progress.current ? progress.current : "Files are only read, never changed, while scanning."}
            </span>
            <button className="btn" onClick={cancel}>Cancel</button>
          </div>
        </div>
      )}

      {outcome && !progress && (
        <div className="banner-info">
          {outcome.cancelled && <>Stopped before finishing. </>}
          {outcome.removed.length > 0 ? (
            <>
              Moved {outcome.removed.length} file{outcome.removed.length === 1 ? "" : "s"} to the {BIN} (
              {formatBytes(outcome.freed_bytes)}). The copies you kept were checked first.{" "}
            </>
          ) : (
            !outcome.cancelled && <>Nothing was moved. </>
          )}
          {outcome.skipped.length > 0 && (
            <>
              Left alone {outcome.skipped.length}:
              <ul className="wt-reasons">
                {outcome.skipped.map((s) => (
                  <li key={s}>{s}</li>
                ))}
              </ul>
            </>
          )}
        </div>
      )}

      {scanning && !report && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Comparing sizes first, then only reading files that could really be the same…</p>
        </div>
      )}

      {report && (
        <>
          {report.cancelled && (
            <div className="banner-info">The scan was cancelled, so no results are shown. Nothing was changed.</div>
          )}

          <section className="metrics">
            <div className="metric accent">
              <span className="metric-label">Could be freed</span>
              <span className="metric-value">{formatBytes(report.wasted_bytes)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Rated Safe</span>
              <span className="metric-value">{formatBytes(report.safe_bytes)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Sets of duplicates</span>
              <span className="metric-value">{groups.length}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Already sharing space</span>
              <span className="metric-value">{formatBytes(report.hard_link_bytes)}</span>
            </div>
          </section>

          {!report.cancelled && (
            <p className="footnote left">
              Looked at {report.files_seen.toLocaleString()} files of {MIN_SIZES.find((m) => m.bytes === report.min_bytes)?.label ?? formatBytes(report.min_bytes)} or
              more in {report.seconds.toFixed(1)} s.
              {report.caches.length > 0 && <> Also compared the caches of {report.caches.join(", ")}.</>}
              {report.online_only_skipped > 0 && (
                <> {report.online_only_skipped} cloud file{report.online_only_skipped === 1 ? " is" : "s are"} stored only online and were not read.</>
              )}
              {report.unreadable_count > 0 && (
                <span title={report.unreadable.join("\n")}>
                  {" "}{report.unreadable_count} file{report.unreadable_count === 1 ? "" : "s"} could not be read (open in another program or no permission); hover for names.
                </span>
              )}
            </p>
          )}

          <div className="banner-info">
            Files count as duplicates only when their whole content is identical (checked with a hash), never by
            name or date. Hard links — several names for one file — are not duplicates and are listed
            separately. Jharu keeps one copy of each set, moves the others to the {BIN}, and reads both again
            right before it does.
          </div>

          {report.hard_links.length > 0 && (
            <details className="dupes-shared">
              <summary>
                {report.hard_links.length} file{report.hard_links.length === 1 ? "" : "s"} with several names
                ({report.hard_links.some((h) => h.clones) ? "hard links or APFS clones" : "hard links"}) already
                share space — {formatBytes(report.hard_link_bytes)} that is not wasted
              </summary>
              {report.hard_links.slice(0, 50).map((h) => (
                <div key={h.paths[0]} className="dupes-shared-set">
                  <strong>
                    {h.name} · {formatBytes(h.size_bytes)} stored once, {h.paths.length} {h.clones ? "clones" : "names"}
                  </strong>
                  {h.paths.map((p) => (
                    <code key={p}>{p}</code>
                  ))}
                </div>
              ))}
            </details>
          )}

          <div className="filter-row">
            <div className="filter-tabs">
              <button
                className={`filter-tab ${filter === "all" ? "is-active" : ""}`}
                onClick={() => { setFilter("all"); setShown(PAGE); }}
              >
                All ({groups.length})
              </button>
              <button
                className={`filter-tab ${filter === "tools" ? "is-active" : ""}`}
                onClick={() => { setFilter("tools"); setShown(PAGE); }}
              >
                ML models and packages ({toolGroups})
              </button>
              <button
                className={`filter-tab ${filter === "files" ? "is-active" : ""}`}
                onClick={() => { setFilter("files"); setShown(PAGE); }}
              >
                Your files ({groups.length - toolGroups})
              </button>
            </div>
            <div className="confirm">
              <button className="btn-ghost sm" onClick={selectAllSafe} disabled={busy}>Select all Safe</button>
              <button className="btn-ghost sm" onClick={() => setSelected(new Set())} disabled={busy || selected.size === 0}>
                Clear
              </button>
              {planCopies.length > 0 && (
                <button className="btn-clean" disabled={busy} onClick={() => setConfirming(true)}>
                  Move {planCopies.length} cop{planCopies.length === 1 ? "y" : "ies"} to {BIN} ({formatBytes(planBytes)})
                </button>
              )}
            </div>
          </div>

          {groups.length === 0 && !report.cancelled && (
            <div className="empty-state">
              <p>No duplicates found in these folders.</p>
            </div>
          )}

          {visible.slice(0, shown).map((g) => {
            const keeper = keeperOf(g);
            return (
              <section key={g.hash} className="dupes-group">
                <div className="dupes-group-head">
                  <div className="dupes-group-title">
                    <div className="finding-title">
                      <span className="finding-name" title={g.name}>{g.name}</span>
                      <span className={`badge cat-${g.kind === "files" ? "app_data" : "ml_models"}`}>{KIND_LABEL[g.kind]}</span>
                    </div>
                    <span className="model-meta">
                      {g.copies.length} copies of {formatBytes(g.size_bytes)} each
                    </span>
                  </div>
                  <div className="dupes-group-side">
                    <span className="size">{formatBytes(g.wasted_bytes)}</span>
                    <button className="btn-ghost sm" disabled={busy} onClick={() => selectGroup(g)}>
                      Select duplicates
                    </button>
                  </div>
                </div>
                {g.copies.map((c) => {
                  const isKeeper = c.path === keeper.path;
                  return (
                    <div key={c.path} className={`dupes-copy ${isKeeper ? "is-kept" : ""}`}>
                      <div className="dupes-pick">
                        <label>
                          <input
                            type="radio"
                            name={`keep-${g.hash}`}
                            checked={isKeeper}
                            disabled={busy}
                            onChange={() => chooseKeeper(g, c)}
                          />
                          Keep
                        </label>
                        <label className={isKeeper || !c.removable ? "is-off" : ""}>
                          <input
                            type="checkbox"
                            checked={!isKeeper && c.removable && selected.has(c.path)}
                            disabled={busy || isKeeper || !c.removable}
                            onChange={() => toggle(c)}
                            title={c.removable ? undefined : "Jharu will not remove this one"}
                          />
                          Remove
                        </label>
                      </div>
                      <div className="dupes-copy-main">
                        <div className="finding-title">
                          <span className="finding-name">{c.place}</span>
                          <span className={`badge safety-${c.safety}`}>{SAFETY_LABEL[c.safety]}</span>
                          {isKeeper && <span className="badge cat-dev_cache">Kept</span>}
                        </div>
                        <code className="finding-path">{c.path}</code>
                        <span className="model-meta">{ageLabel(c.modified_days)}</span>
                        {c.link_paths.length > 0 && (
                          <span className="dupes-links">
                            Same data, also at: {c.link_paths.join(" · ")}
                          </span>
                        )}
                        {isKeeper && c.keep_note && c.keep && !keepOverride[g.hash] && (
                          <p className="dupes-keep-note">{c.keep_note}</p>
                        )}
                        <ul className="safety-reasons">
                          {c.reasons.map((r) => (
                            <li key={r.text} className={`reason-${r.level}`}>
                              <span className="reason-mark" aria-hidden>
                                {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
                              </span>
                              {r.text}
                            </li>
                          ))}
                        </ul>
                      </div>
                      <button
                        className="btn-ghost sm"
                        onClick={() => invoke("reveal_path", { path: c.path }).catch((err) => setError(String(err)))}
                      >
                        Show
                      </button>
                    </div>
                  );
                })}
              </section>
            );
          })}

          {visible.length > shown && (
            <div className="dupes-more">
              <button className="btn" onClick={() => setShown((n) => n + PAGE)}>
                Show more ({visible.length - shown} more sets)
              </button>
            </div>
          )}

          <p className="footnote">
            Safe means the tool rebuilds or re-downloads the copy by itself. Caution covers your own files
            and models a tool will not fetch again for you: the content stays in the copy you keep, but check
            which copy your apps and links use. Copies inside a Git repository, an app or a Python
            environment are never offered for removal, and files inside OneDrive, iCloud, Dropbox and similar
            folders are rated Risky because removing them there removes them everywhere.
          </p>
        </>
      )}
    </>
  );
}
