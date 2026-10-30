import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { WorktreeItem, WorktreeRemoveOutcome, WorktreeReport } from "../types";

type Filter = "all" | "removable" | "keep";

const BIN = isWindows ? "Recycle Bin" : "Trash";

function ageLabel(days: number | null): string {
  if (days == null) return "unknown";
  if (days < 1) return "today";
  if (days < 30) return `${days}d ago`;
  if (days < 365) return `${Math.round(days / 30)} mo ago`;
  return `${(days / 365).toFixed(1)} yr ago`;
}

/// Explanations quote git commands in `backticks`; show those as code.
function withCode(text: string) {
  return text.split("`").map((part, n) => (n % 2 ? <code key={n}>{part}</code> : part));
}

function baseName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

interface Progress {
  done: number;
  total: number;
  current: string;
}

export default function Worktrees({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [report, setReport] = useState<WorktreeReport | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [outcome, setOutcome] = useState<WorktreeRemoveOutcome | null>(null);
  const activeToken = useRef<string | null>(null);

  const items = useMemo(() => report?.repos.flatMap((r) => r.items) ?? [], [report]);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      const r = await invoke<WorktreeReport>("worktree_scan");
      setReport(r);
      setSelected(
        new Set(r.repos.flatMap((repo) => repo.items).filter((i) => i.preselect).map((i) => i.path))
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    scan();
    let unlisten: UnlistenFn | undefined;
    listen<Progress & { token: string }>("worktree-progress", (event) => {
      if (event.payload.token !== activeToken.current) return;
      setProgress(event.payload);
    }).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  const toggle = (item: WorktreeItem) => {
    if (item.verdict === "keep") return;
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(item.path)) next.delete(item.path);
      else next.add(item.path);
      return next;
    });
  };

  const selectedItems = items.filter((i) => selected.has(i.path) && i.verdict !== "keep");
  const selectedBytes = selectedItems.reduce((a, i) => a + i.size_bytes, 0);
  const selectedFolders = selectedItems.filter((i) => i.verdict !== "record");
  const selectedRecords = selectedItems.length - selectedFolders.length;

  const removeSelected = async () => {
    setConfirming(false);
    setOutcome(null);
    const token = `wt:${Math.random()}`;
    activeToken.current = token;
    setProgress({ done: 0, total: selectedItems.length, current: "" });
    try {
      const result = await invoke<WorktreeRemoveOutcome>("remove_worktrees", {
        items: selectedItems.map((i) => ({
          path: i.path,
          common_dir: i.common_dir,
          expected: i.verdict,
        })),
        token,
      });
      setOutcome(result);
      if (result.errors.length) setError(result.errors.join("; "));
      onRecovered(result.freed_bytes);
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      activeToken.current = null;
      setProgress(null);
    }
  };

  const visible = (list: WorktreeItem[]) =>
    filter === "all"
      ? list
      : filter === "keep"
        ? list.filter((i) => i.verdict === "keep")
        : list.filter((i) => i.verdict !== "keep");

  const removableCount = items.filter((i) => i.verdict !== "keep").length;
  const keepCount = items.length - removableCount;
  const folderCount = items.filter((i) => i.verdict !== "record").length;

  const title =
    selectedFolders.length > 0
      ? `Remove ${selectedFolders.length} worktree${selectedFolders.length === 1 ? "" : "s"}?`
      : `Clear ${selectedRecords} stale record${selectedRecords === 1 ? "" : "s"}?`;

  return (
    <>
      {confirming && (
        <ConfirmClean
          request={{
            kind: "worktree",
            title,
            items: selectedItems.map((i) =>
              i.verdict === "record" ? `${i.path} (record only)` : i.path
            ),
            bytes: selectedBytes,
            risky: selectedItems.some((i) => i.verdict === "probably_merged" || i.owner != null),
          }}
          busy={progress != null}
          onConfirm={removeSelected}
          onCancel={() => setConfirming(false)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Git worktrees</h2>
          <p className="view-sub">
            Spare checkouts left by <code>git worktree</code>, Claude Code and Codex — remove the
            ones already merged
          </p>
        </div>
        <button className="btn" onClick={scan} disabled={scanning || progress != null}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {progress && (
        <div className="banner-info wt-progress">
          <div className="meter-head">
            <span>
              Moving worktrees to the {BIN}
              {progress.current && ` · ${progress.current}`}
            </span>
            <span className="meter-value">
              {Math.min(progress.done, progress.total)} of {progress.total}
            </span>
          </div>
          <div className="meter-track">
            <div
              className="meter-fill"
              style={{
                width: `${Math.max(3, (progress.done / (progress.total || 1)) * 100)}%`,
                background: "var(--accent)",
              }}
            />
          </div>
          <p className="model-meta">
            Large folders can take a while on {isWindows ? "Windows" : "this disk"}. You can keep
            using Jharu in the meantime.
          </p>
        </div>
      )}

      {outcome && !progress && (
        <div className="banner-info">
          {outcome.removed.length > 0 && (
            <>
              Moved {outcome.removed.length} worktree{outcome.removed.length === 1 ? "" : "s"} to the{" "}
              {BIN} ({formatBytes(outcome.freed_bytes)}). Their branches are still in git.{" "}
            </>
          )}
          {outcome.already_gone.length > 0 && (
            <>
              {outcome.already_gone.length} had already disappeared — usually because the tool
              that made it archived its session — and git's record of it was cleared.{" "}
            </>
          )}
          {outcome.skipped.length > 0 && (
            <>
              Skipped {outcome.skipped.length}:
              <ul className="wt-reasons">
                {outcome.skipped.map((s) => (
                  <li key={s}>{withCode(s)}</li>
                ))}
              </ul>
            </>
          )}
          {outcome.removed.length === 0 &&
            outcome.already_gone.length === 0 &&
            outcome.skipped.length === 0 &&
            "Stale records cleared."}
        </div>
      )}

      {scanning && !report && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Looking for repositories and checking each worktree with git…</p>
        </div>
      )}

      {report && !report.git_available && (
        <div className="banner-info">
          Jharu needs git to check whether a worktree's work is merged, and could not find it.
          Install git{isWindows ? " (git-scm.com)" : " (Xcode command line tools or Homebrew)"}, then
          rescan.
        </div>
      )}

      {report && (
        <>
          <section className="metrics">
            <div className="metric">
              <span className="metric-label">All worktrees</span>
              <span className="metric-value">{formatBytes(report.total_bytes)}</span>
            </div>
            <div className="metric accent">
              <span className="metric-label">Merged and clean</span>
              <span className="metric-value">{formatBytes(report.safe_bytes)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Worktree folders</span>
              <span className="metric-value">{folderCount}</span>
            </div>
          </section>

          <div className="banner-info">
            A worktree is a second checkout of a repository, often made by an AI coding agent for one
            task. Once its branch is merged it is just a full copy taking space. Jharu moves the folder
            to the {BIN} and never deletes a branch, so any of them can be recreated with{" "}
            <code>git worktree add</code>.
          </div>

          <div className="filter-row">
            <div className="filter-tabs">
              <button
                className={`filter-tab ${filter === "all" ? "is-active" : ""}`}
                onClick={() => setFilter("all")}
              >
                All ({items.length})
              </button>
              <button
                className={`filter-tab ${filter === "removable" ? "is-active" : ""}`}
                onClick={() => setFilter("removable")}
              >
                Removable ({removableCount})
              </button>
              <button
                className={`filter-tab ${filter === "keep" ? "is-active" : ""}`}
                onClick={() => setFilter("keep")}
              >
                Keep ({keepCount})
              </button>
            </div>
            <div className="confirm">
              {selectedItems.length > 0 && (
                <button
                  className="btn-clean"
                  disabled={progress != null}
                  onClick={() => setConfirming(true)}
                >
                  {selectedFolders.length > 0
                    ? `Remove ${selectedFolders.length} selected (${formatBytes(selectedBytes)})`
                    : `Clear ${selectedRecords} stale record${selectedRecords === 1 ? "" : "s"}`}
                </button>
              )}
            </div>
          </div>

          {report.repos.length === 0 && (
            <div className="empty-state">
              <p>No extra worktrees found — every repository has just its main checkout.</p>
            </div>
          )}

          {report.repos.map((repo) => {
            const shown = visible(repo.items);
            if (shown.length === 0) return null;
            return (
              <section key={repo.common_dir || repo.repo}>
                <h2 className="section-title">
                  {repo.common_dir ? baseName(repo.repo) : repo.repo}{" "}
                  <span className="count">{repo.items.length}</span>
                </h2>
                {repo.common_dir && (
                  <p className="view-sub wt-repo-sub">
                    <code>{repo.repo}</code>
                    {repo.base && <> · compared with {repo.base}</>} · {formatBytes(repo.total_bytes)}
                  </p>
                )}
                <div className="findings">
                  {shown.map((i) => (
                    <label
                      key={i.path}
                      className={`model-row wt-row ${i.verdict === "keep" ? "is-locked" : ""}`}
                    >
                      <input
                        type="checkbox"
                        checked={selected.has(i.path)}
                        disabled={i.verdict === "keep" || progress != null}
                        onChange={() => toggle(i)}
                        title={i.verdict === "keep" ? "Jharu will not remove this one" : undefined}
                      />
                      <div className="model-main">
                        <div className="finding-title">
                          <span className="finding-name">{i.name}</span>
                          <span className={`badge safety-${i.safety}`}>{i.summary}</span>
                          {i.owner && <span className="badge cat-dev_cache">{i.owner}</span>}
                        </div>
                        <span className="model-meta">
                          {i.verdict === "record"
                            ? "Folder already deleted"
                            : i.branch
                              ? `Branch ${i.branch}`
                              : i.head
                                ? `Detached HEAD at ${i.head}`
                                : "No commit"}
                          {i.last_commit_days != null &&
                            ` · last commit ${ageLabel(i.last_commit_days)}`}
                          {i.last_commit_subject && `: ${i.last_commit_subject}`}
                          {i.modified_days != null && ` · files changed ${ageLabel(i.modified_days)}`}
                        </span>
                        <code className="finding-path">{i.path}</code>
                        <p className="wt-explain">{withCode(i.explanation)}</p>
                        {i.reasons.length > 0 && (
                          <ul className="wt-reasons">
                            {i.reasons.map((r) => (
                              <li key={r}>{withCode(r)}</li>
                            ))}
                          </ul>
                        )}
                      </div>
                      <div className="finding-size">
                        <span className="size">
                          {i.verdict === "record" ? "—" : formatBytes(i.size_bytes)}
                        </span>
                      </div>
                      {i.verdict !== "record" && (
                        <button
                          className="btn-ghost sm"
                          onClick={(e) => {
                            e.preventDefault();
                            invoke("reveal_path", { path: i.path }).catch((err) =>
                              setError(String(err))
                            );
                          }}
                        >
                          Show
                        </button>
                      )}
                    </label>
                  ))}
                </div>
              </section>
            );
          })}

          <p className="footnote">
            "Probably merged" means the same changes are on the main branch under different commit
            IDs, as after a rebase or cherry-pick. A squash merge of several commits cannot be
            detected this way, so those stay under Keep. Worktrees made by Claude Code or Codex are
            never selected automatically, because the tool may still be using them.
          </p>
        </>
      )}
    </>
  );
}
