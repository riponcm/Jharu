/// Some help text names a platform-specific tool or settings pane, so it has to
/// know which platform it is being read on.
export const isWindows = /Windows/i.test(navigator.userAgent);

export function formatBytes(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} MB`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(0)} KB`;
  return `${n} B`;
}
