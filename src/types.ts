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
  read_error: string | null;
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
  uninstall_command: string | null;
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

export interface ModelEntry {
  provider: string;
  name: string;
  size_bytes: number;
  exclusive_bytes: number;
  paths: string[];
  downloaded_days: number | null;
  last_used_days: number | null;
  used_since_download: boolean;
  note: string | null;
}

export interface ModelReport {
  total_bytes: number;
  unused_bytes: number;
  usage_tracking_reliable: boolean;
  models: ModelEntry[];
}

export interface PyPackage {
  name: string;
  path: string;
  size_bytes: number;
}

export interface PyEnv {
  name: string;
  path: string;
  kind: string;
  size_bytes: number;
  stale_days: number | null;
  packages: PyPackage[];
}

export interface PyDuplicate {
  name: string;
  copies: number;
  total_bytes: number;
  wasted_bytes: number;
}

export interface PyReport {
  total_bytes: number;
  wasted_bytes: number;
  envs: PyEnv[];
  duplicates: PyDuplicate[];
}

export interface DesktopItem {
  name: string;
  path: string;
  size_bytes: number;
  age_days: number | null;
  is_screenshot: boolean;
}

export interface DesktopReport {
  desktop_path: string;
  total_bytes: number;
  file_count: number;
  screenshot_bytes: number;
  screenshot_count: number;
  suggested_archive: string;
  items: DesktopItem[];
}

export interface MoveOutcome {
  moved: number;
  moved_bytes: number;
  errors: string[];
}

export interface LeftoverEntry {
  label: string;
  path: string;
  size_bytes: number;
  confident: boolean;
}

export interface SmartDetail {
  temperature_c: number | null;
  power_on_hours: number | null;
  percentage_used: number | null;
  reallocated_sectors: number | null;
}

export interface HistoryPoint {
  day: number;
  used: number;
}

export interface Forecast {
  points: HistoryPoint[];
  days_until_full: number | null;
  bytes_per_day: number;
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
  smart_detail: SmartDetail | null;
  forecast: Forecast | null;
}

export interface HealthReport {
  drives: DriveInfo[];
  smart_tool_available: boolean;
}
