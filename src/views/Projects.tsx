import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { Safety } from "../types";
import type {
  BuildFolder,
  BuildProgress,
  BuildProject,
  BuildRemoveOutcome,
  BuildReport,
} from "../types/projects";
import "../styles/projects.css";

type AgeFilter = 0 | 30 | 90 | 180;
type Sort = "size" | "age";

const BIN = isWindows ? "Recycle Bin" : "Trash";
const ROOTS_KEY = "jharu.build-roots";

const SAFETY_LABEL: Record<Safety, string> = {
  safe: "Safe",
  caution: "Caution",
  risky: "Review first",
};

function ageLabel(days: number | null): string {
  if (days == null) return "unknown";
  if (days < 1) return "today";
  if (days < 30) return `${days}d ago`;
  if (days < 365) return `${Math.round(days / 30)} mo ago`;
  return `${(days / 365).toFixed(1)} yr ago`;
}

function baseName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

function folderName(f: BuildFolder): string {
  return f.paths.length > 1 ? `${f.paths.length} × ${baseName(f.id)}` : baseName(f.id);
}

function loadRoots(): string[] {
  try {
    const raw = localStorage.getItem(ROOTS_KEY);
    const parsed = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed.filter((r) => typeof r === "string") : [];
  } catch {
    return [];
  }
}

function saveRoots(roots: string[]) {
  try {
    localStorage.setItem(ROOTS_KEY, JSON.stringify(roots));
  } catch {
    // Remembering the roots is a convenience; the scan works without it.
  }
}

function phaseText(p: BuildProgress): string {
  switch (p.phase) {
    case "find":
      return "Looking for projects";
    case "size":
      return "Measuring build folders";
    case "source":
      return "Checking when each project last changed";
    case "remove":
      return `Moving build folders to the ${BIN}`;
  }
}

export default function Projects({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [report, setReport] = useState<BuildReport | null>(null);
  const [scanning, setScanning] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [roots, setRoots] = useState<string[]>(loadRoots);
  const [rootInput, setRootInput] = useState("");
  const [age, setAge] = useState<AgeFilter>(0);
  const [sort, setSort] = useState<Sort>("size");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [progress, setProgress] = useState<BuildProgress | null>(null);
  const [outcome, setOutcome] = useState<BuildRemoveOutcome | null>(null);
  const activeToken = useRef<string | null>(null);

  const busy = scanning || removing;

  const scan = async () => {
    const token = `bf:${Math.random()}`;
    activeToken.current = token;
    setScanning(true);
    setCancelling(false);
    setError(null);
    setProgress({ token, phase: "find", done: 0, total: 0, current: "" });
    try {
      const r = await invoke<BuildReport>("build_scan", { extraRoots: roots, token });
      setReport(r);
      setSelected(
        new Set(r.projects.flatMap((p) => p.folders).filter((f) => f.preselect).map((f) => f.id))
      );
    } catch (e) {
      const text = String(e);
      if (!text.includes("cancelled")) setError(text);
      else setError("Scan cancelled. Nothing was changed.");
    } finally {
      activeToken.current = null;
      setScanning(false);
      setProgress(null);
    }
  };

  useEffect(() => {
    scan();
    let unlisten: UnlistenFn | undefined;
    listen<BuildProgress>("build-folders-progress", (event) => {
      if (event.payload.token !== activeToken.current) return;
      setProgress(event.payload);
    }).then((u) => (unlisten = u));
    return () => unlisten?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const cancel = () => {
    if (!activeToken.current) return;
    setCancelling(true);
    invoke("cancel_build_work", { token: activeToken.current }).catch(() => {});
  };

  const addRoot = () => {
    const value = rootInput.trim();
    if (!value || roots.includes(value)) return;
    const next = [...roots, value];
    setRoots(next);
    saveRoots(next);
    setRootInput("");
  };

  const dropRoot = (root: string) => {
    const next = roots.filter((r) => r !== root);
    setRoots(next);
    saveRoots(next);
  };

  const projects = useMemo(() => {
    const list = (report?.projects ?? []).filter(
      (p) => age === 0 || (p.source_modified_days != null && p.source_modified_days >= age)
    );
    return [...list].sort((a, b) =>
      sort === "size"
        ? b.total_bytes - a.total_bytes
        : (b.source_modified_days ?? -1) - (a.source_modified_days ?? -1)
    );
  }, [report, age, sort]);

  const visibleFolders = projects.flatMap((p) => p.folders);
  const selectedFolders = visibleFolders.filter((f) => selected.has(f.id) && !f.in_use);
  const selectedBytes = selectedFolders.reduce((a, f) => a + f.size_bytes, 0);
  const allProjects = report?.projects ?? [];
  const safeVisible = visibleFolders.filter((f) => f.safety === "safe" && !f.in_use);

  const toggle = (f: BuildFolder) => {
    if (f.in_use) return;
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(f.id)) next.delete(f.id);
      else next.add(f.id);
      return next;
    });
  };

  const removeSelected = async () => {
    setConfirming(false);
    setOutcome(null);
    setError(null);
    const token = `bf:${Math.random()}`;
    activeToken.current = token;
    setCancelling(false);
    setRemoving(true);
    setProgress({ token, phase: "remove", done: 0, total: selectedFolders.length, current: "" });
    try {
      const result = await invoke<BuildRemoveOutcome>("remove_build_folders", {
        items: selectedFolders.map((f) => ({
          project: f.project,
          kind: f.kind,
          paths: f.paths,
          expected: f.safety,
        })),
        token,
      });
      setOutcome(result);
      if (result.errors.length) setError(result.errors.join("; "));
      onRecovered(result.freed_bytes);
      // Drop what left the list instead of walking the whole disk again.
      const gone = new Set([...result.removed, ...result.already_gone]);
      setReport((prev) => (prev ? withoutFolders(prev, gone) : prev));
      setSelected((prev) => new Set([...prev].filter((id) => !gone.has(id))));
    } catch (e) {
      setError(String(e));
    } finally {
      activeToken.current = null;
      setRemoving(false);
      setProgress(null);
    }
  };

  const folderCount = allProjects.reduce((a, p) => a + p.folders.length, 0);
  const title = `Move ${selectedFolders.length} build folder${selectedFolders.length === 1 ? "" : "s"} to the ${BIN}?`;

  return (
    <>
      {confirming && (
        <ConfirmClean
          request={{
            kind: "build",
            title,
            items: selectedFolders.map((f) =>
              f.paths.length > 1 ? `${f.id} (+${f.paths.length - 1} more)` : f.id
            ),
            bytes: selectedBytes,
            risky: selectedFolders.some((f) => f.safety === "risky"),
          }}
          busy={removing}
          onConfirm={removeSelected}
          onCancel={() => setConfirming(false)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Build folders</h2>
          <p className="view-sub">
            <code>node_modules</code>, Rust <code>target</code>, <code>.next</code>, virtualenvs and
            other build output inside your projects — each one confirmed by the project beside it
          </p>
        </div>
        <button className="btn" onClick={scan} disabled={busy}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      <div className="bf-roots">
        <span>Searching your home folder{roots.length > 0 ? " and" : ""}</span>
        {roots.map((r) => (
          <span key={r} className="bf-root-chip">
            <code>{r}</code>
            <button onClick={() => dropRoot(r)} disabled={busy} title="Stop searching here" aria-label={`Remove ${r}`}>
              ×
            </button>
          </span>
        ))}
        <input
          className="bf-root-input"
          value={rootInput}
          placeholder={isWindows ? "Add a folder, e.g. D:\\code" : "Add a folder, e.g. /Volumes/Work/code"}
          onChange={(e) => setRootInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && addRoot()}
        />
        <button className="btn-ghost sm" onClick={addRoot} disabled={!rootInput.trim()}>
          Add
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {progress && (
        <div className="banner-info bf-progress">
          <div className="meter-head">
            <span>
              {phaseText(progress)}
              {progress.current && ` · ${progress.current}`}
            </span>
            {progress.total > 0 && (
              <span className="meter-value">
                {Math.min(progress.done, progress.total)} of {progress.total}
              </span>
            )}
          </div>
          {progress.total > 0 && (
            <div className="meter-track">
              <div
                className="meter-fill"
                style={{
                  width: `${Math.max(3, (progress.done / progress.total) * 100)}%`,
                  background: "var(--accent)",
                }}
              />
            </div>
          )}
          <div className="bf-progress-actions">
            <p className="model-meta">
              {progress.phase === "remove"
                ? `Large folders can take a while. Cancel stops after the folder it is on; nothing already in the ${BIN} is touched.`
                : "Scanning only reads your disk. You can keep using Jharu in the meantime."}
            </p>
            <button className="btn" onClick={cancel} disabled={cancelling}>
              {cancelling ? "Cancelling…" : "Cancel"}
            </button>
          </div>
        </div>
      )}

      {outcome && !removing && (
        <div className="banner-info">
          {outcome.removed.length > 0 && (
            <>
              Moved {outcome.removed.length} build folder{outcome.removed.length === 1 ? "" : "s"} to
              the {BIN} ({formatBytes(outcome.freed_bytes)}). Your source files were not touched.{" "}
            </>
          )}
          {outcome.already_gone.length > 0 && (
            <>{outcome.already_gone.length} had already disappeared. </>
          )}
          {outcome.cancelled && (
            <>Cancelled: {outcome.not_started} folder{outcome.not_started === 1 ? "" : "s"} not started. </>
          )}
          {outcome.skipped.length > 0 && (
            <>
              Skipped {outcome.skipped.length}:
              <ul className="wt-reasons">
                {outcome.skipped.map((s) => (
                  <li key={s}>{s}</li>
                ))}
              </ul>
            </>
          )}
          {outcome.removed.length === 0 &&
            outcome.already_gone.length === 0 &&
            outcome.skipped.length === 0 &&
            !outcome.cancelled &&
            "Nothing was moved."}
        </div>
      )}

      {report && report.skipped_roots.length > 0 && (
        <div className="banner-info">
          Not searched:
          <ul className="wt-reasons">
            {report.skipped_roots.map((s) => (
              <li key={s}>{s}</li>
            ))}
          </ul>
        </div>
      )}

      {scanning && !report && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Looking through your projects for build folders…</p>
        </div>
      )}

      {report && (
        <>
          <section className="metrics">
            <div className="metric">
              <span className="metric-label">All build folders</span>
              <span className="metric-value">{formatBytes(report.total_bytes)}</span>
            </div>
            <div className="metric accent">
              <span className="metric-label">Rated safe</span>
              <span className="metric-value">{formatBytes(report.safe_bytes)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Projects</span>
              <span className="metric-value">
                {allProjects.length} · {folderCount} folder{folderCount === 1 ? "" : "s"}
              </span>
            </div>
          </section>

          <div className="banner-info">
            Jharu offers a folder only when the project beside it confirms what it is, such as a{" "}
            <code>package.json</code> next to <code>node_modules</code> or a <code>Cargo.toml</code> next
            to <code>target</code>. It moves folders to the {BIN}, never your source, and checks each one
            again just before moving it. Anything a dev server or build tool is using is left alone.
          </div>

          <div className="filter-row">
            <div className="filter-tabs" role="group" aria-label="Project age">
              {([0, 30, 90, 180] as AgeFilter[]).map((d) => (
                <button
                  key={d}
                  className={`filter-tab ${age === d ? "is-active" : ""}`}
                  onClick={() => setAge(d)}
                >
                  {d === 0 ? "All projects" : `Untouched ${d}+ days`}
                </button>
              ))}
            </div>
            <div className="bf-toolbar">
              <label className="model-meta">
                Sort by{" "}
                <select value={sort} onChange={(e) => setSort(e.target.value as Sort)}>
                  <option value="size">Size</option>
                  <option value="age">Longest untouched</option>
                </select>
              </label>
              <button
                className="btn-ghost sm"
                disabled={busy || safeVisible.length === 0}
                onClick={() => setSelected(new Set(safeVisible.map((f) => f.id)))}
              >
                Select all safe
              </button>
              <button
                className="btn-ghost sm"
                disabled={busy || selected.size === 0}
                onClick={() => setSelected(new Set())}
              >
                Clear
              </button>
              {selectedFolders.length > 0 && (
                <button className="btn-clean" disabled={busy} onClick={() => setConfirming(true)}>
                  {`Remove ${selectedFolders.length} selected (${formatBytes(selectedBytes)})`}
                </button>
              )}
            </div>
          </div>

          {projects.length === 0 && (
            <div className="empty-state">
              <p>
                {allProjects.length === 0
                  ? "No build folders found in your projects."
                  : "No projects match this filter."}
              </p>
            </div>
          )}

          {projects.map((p) => (
            <ProjectCard
              key={p.path}
              project={p}
              selected={selected}
              busy={busy}
              onToggle={toggle}
              onError={setError}
            />
          ))}

          <p className="footnote">
            Folders are pre-selected only when they are rated Safe and the project's own files have not
            changed for 30 days. Safe means the build tool brings the folder back by itself, or a
            lockfile makes the reinstall identical. Virtual environments and CMake folders are rated
            Caution because they need a reinstall or the original configure options. "Last changed"
            looks at the project's source files, not at its build output.
          </p>
          {isWindows && (
            <p className="footnote">
              Windows deletes a folder for good, without asking, when it is larger than the Recycle
              Bin's size limit or the Recycle Bin is turned off for that drive. Check the limit in the
              Recycle Bin's Properties before removing very large folders.
            </p>
          )}
        </>
      )}
    </>
  );
}

function withoutFolders(report: BuildReport, gone: Set<string>): BuildReport {
  const projects = report.projects
    .map((p) => {
      const folders = p.folders.filter((f) => !gone.has(f.id));
      return { ...p, folders, total_bytes: folders.reduce((a, f) => a + f.size_bytes, 0) };
    })
    .filter((p) => p.folders.length > 0);
  const all = projects.flatMap((p) => p.folders);
  return {
    ...report,
    projects,
    total_bytes: all.reduce((a, f) => a + f.size_bytes, 0),
    safe_bytes: all.filter((f) => f.safety === "safe" && !f.in_use).reduce((a, f) => a + f.size_bytes, 0),
  };
}

function ProjectCard({
  project,
  selected,
  busy,
  onToggle,
  onError,
}: {
  project: BuildProject;
  selected: Set<string>;
  busy: boolean;
  onToggle: (f: BuildFolder) => void;
  onError: (message: string) => void;
}) {
  return (
    <section className="bf-project">
      <div className="bf-project-head">
        <h3 className="bf-project-name">
          {project.name} <span className="count">{project.folders.length}</span>
        </h3>
        <span className="size">{formatBytes(project.total_bytes)}</span>
      </div>
      <p className="bf-project-meta">
        {project.project_type} · source files last changed {ageLabel(project.source_modified_days)} ·{" "}
        <code>{project.path}</code>
      </p>
      <div className="findings">
        {project.folders.map((f) => (
          <label key={f.id} className={`model-row bf-row ${f.in_use ? "is-locked" : ""}`}>
            <input
              type="checkbox"
              checked={selected.has(f.id) && !f.in_use}
              disabled={f.in_use || busy}
              onChange={() => onToggle(f)}
              title={f.in_use ? "In use right now, so Jharu will not remove it" : undefined}
            />
            <div className="model-main">
              <div className="finding-title">
                <span className="finding-name">{folderName(f)}</span>
                <span className="badge cat-dev_cache">{f.label}</span>
                <span className={`badge safety-${f.safety}`}>{SAFETY_LABEL[f.safety]}</span>
                {f.in_use && <span className="badge safety-caution">In use</span>}
              </div>
              <code className="finding-path">{f.id}</code>
              <p className="bf-rebuild">
                Comes back with <code>{f.rebuild}</code>
              </p>
              <ul className="safety-reasons">
                {f.reasons.map((r) => (
                  <li key={r.text} className={`reason-${r.level}`}>
                    <span className="reason-mark" aria-hidden>
                      {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
                    </span>
                    {r.text}
                  </li>
                ))}
              </ul>
            </div>
            <div className="finding-size">
              <span className="size">{formatBytes(f.size_bytes)}</span>
            </div>
            <button
              className="btn-ghost sm"
              onClick={(e) => {
                e.preventDefault();
                invoke("reveal_path", { path: f.id }).catch((err) => onError(String(err)));
              }}
            >
              Show
            </button>
          </label>
        ))}
      </div>
    </section>
  );
}
