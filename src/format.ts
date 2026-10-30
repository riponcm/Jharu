/// Some help text names a platform-specific tool or settings pane, so it has to
/// know which platform it is being read on.
export const isWindows = /Windows/i.test(navigator.userAgent);
export const isMac = /Macintosh|Mac OS X/i.test(navigator.userAgent);

/// Sizes in the units the file manager uses, so Jharu's numbers can be
/// compared with it directly: Explorer counts 1 GB as 1024³ bytes, Finder as
/// 1000³. Without this the same drive read 7% fuller in Jharu on Windows.
const UNIT = isWindows ? 1024 : 1000;

export function formatBytes(n: number): string {
  if (n >= UNIT ** 3) return `${(n / UNIT ** 3).toFixed(1)} GB`;
  if (n >= UNIT ** 2) return `${(n / UNIT ** 2).toFixed(1)} MB`;
  if (n >= UNIT) return `${(n / UNIT).toFixed(0)} KB`;
  return `${n} B`;
}
