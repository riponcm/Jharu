export type Category = "ml_models" | "dev_cache" | "app_data";
export type Safety = "safe" | "caution" | "risky";

export interface ChildEntry {
  name: string;
  path: string;
  size_bytes: number;
}

export interface Finding {
  rule_id: string;
  name: string;
  description: string;
  category: Category;
  safety: Safety;
  path: string;
  size_bytes: number;
  file_count: number;
  stale_days: number | null;
  children: ChildEntry[];
}

export interface ScanResult {
  disk_total: number;
  disk_free: number;
  findings: Finding[];
}

export interface CleanOutcome {
  freed_bytes: number;
  errors: string[];
}

export interface DeepEntry {
  name: string;
  path: string;
  is_dir: boolean;
  size_bytes: number;
  file_count: number;
  kind: "system" | "applications" | "user_data" | "app_data" | "dev" | "other";
  protected: boolean;
  hint: string;
}

export interface BrowserItem {
  label: string;
  path: string;
  size_bytes: number;
  safe: boolean;
}

export interface BrowserReport {
  name: string;
  total_bytes: number;
  items: BrowserItem[];
}

export interface AppInfo {
  name: string;
  path: string;
  size_bytes: number;
  data_bytes: number;
  bundle_id: string | null;
  version: string | null;
  last_used_days: number | null;
}

export interface JunkItem {
  label: string;
  path: string;
  size_bytes: number;
  stale_days: number | null;
  safe: boolean;
}

export interface JunkGroup {
  id: string;
  title: string;
  note: string;
  cleanable: boolean;
  total_bytes: number;
  items: JunkItem[];
}

export interface AccessStatus {
  full_disk: boolean;
  needs_prompt: boolean;
}

export interface LeftoverEntry {
  label: string;
  path: string;
  size_bytes: number;
  confident: boolean;
}

export interface DriveInfo {
  name: string;
  mount: string;
  file_system: string;
  kind: string;
  total_bytes: number;
  available_bytes: number;
  removable: boolean;
  smart_status: string | null;
}
