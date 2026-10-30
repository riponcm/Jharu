import type { DeepEntry } from "../types";

export type Verdict = "safe" | "caution" | "risky" | "unknown" | "protected";

// Same wording as the badges in Deep clean's list, so a box and its row agree.
export const VERDICT_LABEL: Record<Verdict, string> = {
  safe: "Safe to remove",
  caution: "Remove with care",
  risky: "Review first",
  unknown: "Unknown — look inside",
  protected: "Protected",
};

export interface DeepVerdict {
  level: Verdict;
  reasons: string[];
}

/// The colour and tooltip for one box. The backend rates every folder that is
/// not protected (places.rs: known location, then contents, then live checks)
/// and that rating is used as is. Protected folders and the "not measurable"
/// summary row carry no rating, so they get a fixed one here.
export function deepVerdict(entry: DeepEntry): DeepVerdict {
  const v = rate(entry);
  // An empty hint would render as a blank bullet.
  return { level: v.level, reasons: v.reasons.filter((r) => r.trim().length > 0) };
}

function rate(entry: DeepEntry): DeepVerdict {
  if (entry.is_virtual) {
    return { level: "protected", reasons: [entry.hint, "Not a folder: space in use that could not be measured."] };
  }
  if (entry.protected) {
    return {
      level: "protected",
      reasons: [entry.hint, "Jharu never cleans this location; you can still look inside it."],
    };
  }
  const v = entry.verdict;
  if (!v) {
    return { level: "unknown", reasons: [entry.hint, "Not rated. Look inside before deciding."] };
  }
  const reasons = [v.what ?? "", ...v.reasons.map((r) => r.text)];
  if (!v.clean_here) reasons.push("Deep clean does not remove this; the reasons say where to clean it.");
  return { level: v.basis === "unknown" ? "unknown" : v.safety, reasons };
}
