import { formatBytes } from "../format";

export type CleanKind =
  | "cache"
  | "model"
  | "environment"
  | "userfile"
  | "app"
  | "worktree"
  | "build"
  | "sandbox"
  | "system";

export interface ConfirmRequest {
  kind: CleanKind;
  title: string;
  /// What will actually be removed, shown as a list.
  items: string[];
  bytes: number;
  /// Extra caution text for items Jharu rates as needing review.
  risky?: boolean;
  /// Locations that need administrator permission; Windows will ask for it.
  admin?: string[];
}

const CONSEQUENCE: Record<CleanKind, string> = {
  cache: "This is a cache. Your tools rebuild it automatically the next time they need it, so nothing breaks — the only cost is that the next build or install is slower once.",
  model: "These model files can be downloaded again at any time. If a project still refers to this model, it will simply be fetched again the next time your code runs. The only cost is the download.",
  environment:
    "Removing an environment deletes the packages installed inside it. Any project still using it will need the environment recreated before it runs again.",
  userfile:
    "These are your own files, not cache. Nothing will download them again. Archive them instead if you are not certain you want them gone.",
  app: "This removes the application along with the support files you selected. Anything the app stored that you did not select stays on disk.",
  system:
    "These are temporary files, crash reports and download caches that the system and your programs create again as needed. Jharu moves what is inside each folder and keeps the folder itself. Anything a running program has open, or that changed in the last day, stays where it is.",
  worktree:
    "These are spare checkouts of a repository. Their branches stay in git, so git worktree add recreates any of them. Jharu checks each one again just before moving it and skips any that gained new work since the scan, then runs git worktree prune so git forgets the folders that are gone.",
  build:
    "These are folders your build tools create inside your projects: installed packages, compiler output and caches. Your source files are not touched. Each one comes back by running its install or build command, listed for every folder; virtual environments and CMake folders need extra steps and are rated Caution. Jharu checks each folder again just before moving it and skips any that a dev server or build tool is using.",
  sandbox:
    "These are folders inside the private storage of apps from the Microsoft Store or the Mac App Store. Caches and temporary files are made again by the app; anything marked Caution (sign-in cookies, data of an app that is no longer installed) is not. Jharu checks each folder again just before moving it, skips any whose app is running, and keeps the folders an app expects to find, moving only what is inside them.",
};

export default function ConfirmClean({
  request,
  onConfirm,
  onCancel,
  busy,
}: {
  request: ConfirmRequest;
  onConfirm: () => void;
  onCancel: () => void;
  busy?: boolean;
}) {
  const many = request.items.length > 4;
  const shown = many ? request.items.slice(0, 4) : request.items;

  return (
    <div className="modal-backdrop" role="dialog" aria-modal="true" onClick={onCancel}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3 className="modal-title">{request.title}</h3>

        <p className="modal-body">{CONSEQUENCE[request.kind]}</p>

        {request.risky && (
          <p className="modal-warn">
            Jharu rates this one as worth reviewing first — it may hold data that is not
            reproducible. Open it and check before you continue.
          </p>
        )}

        {request.admin && request.admin.length > 0 && (
          <p className="modal-body">
            Windows will ask for administrator permission once, to clean{" "}
            {request.admin.join(", ")}. Jharu itself keeps running without it.
          </p>
        )}

        <div className="modal-items">
          {shown.map((i) => (
            <code key={i} className="modal-item">{i}</code>
          ))}
          {many && (
            <span className="modal-more">and {request.items.length - 4} more</span>
          )}
        </div>

        <div className="modal-total">
          <span>Space reclaimed</span>
          <strong>{formatBytes(request.bytes)}</strong>
        </div>

        <p className="modal-foot">
          Everything moves to the Trash, so you can put it back until you empty it.
        </p>

        <div className="modal-actions">
          <button className="btn" onClick={onCancel} disabled={busy}>
            No, keep it
          </button>
          <button className="btn-danger" onClick={onConfirm} disabled={busy}>
            {busy ? "Working…" : "Yes, move to Trash"}
          </button>
        </div>
      </div>
    </div>
  );
}
