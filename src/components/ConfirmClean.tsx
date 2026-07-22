import { formatBytes } from "../format";

export type CleanKind = "cache" | "model" | "environment" | "userfile" | "app";

export interface ConfirmRequest {
  kind: CleanKind;
  title: string;
  /// What will actually be removed, shown as a list.
  items: string[];
  bytes: number;
  /// Extra caution text for items Jharu rates as needing review.
  risky?: boolean;
}

const CONSEQUENCE: Record<CleanKind, string> = {
  cache: "This is a cache. Your tools rebuild it automatically the next time they need it, so nothing breaks — the only cost is that the next build or install is slower once.",
  model: "These model files can be downloaded again at any time. If a project still refers to this model, it will simply be fetched again the next time your code runs. The only cost is the download.",
  environment:
    "Removing an environment deletes the packages installed inside it. Any project still using it will need the environment recreated before it runs again.",
  userfile:
    "These are your own files, not cache. Nothing will download them again. Archive them instead if you are not certain you want them gone.",
  app: "This removes the application along with the support files you selected. Anything the app stored that you did not select stays on disk.",
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
