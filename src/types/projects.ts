import type { Reason, Safety } from "../types";

export type BuildKind =
  | "node_modules"
  | "next"
  | "nuxt"
  | "svelte_kit"
  | "turbo"
  | "parcel_cache"
  | "rust_target"
  | "maven_target"
  | "build_output"
  | "py_cache"
  | "pytest_cache"
  | "mypy_cache"
  | "ruff_cache"
  | "tox"
  | "gradle_build"
  | "gradle_cache"
  | "dart_tool"
  | "pods"
  | "cmake"
  | "venv";

/// One removable thing: a build folder, or all of a project's __pycache__ folders.
export interface BuildFolder {
  /// The first path; a stable key.
  id: string;
  paths: string[];
  project: string;
  kind: BuildKind;
  label: string;
  size_bytes: number;
  safety: Safety;
  reasons: Reason[];
  /// The command that brings it back.
  rebuild: string;
  in_use: boolean;
  preselect: boolean;
}

export interface BuildProject {
  path: string;
  name: string;
  project_type: string;
  /// Days since the project's own files (not its build folders) changed.
  source_modified_days: number | null;
  total_bytes: number;
  folders: BuildFolder[];
}

export interface BuildReport {
  total_bytes: number;
  safe_bytes: number;
  roots: string[];
  skipped_roots: string[];
  projects: BuildProject[];
}

export interface BuildRemoveOutcome {
  freed_bytes: number;
  removed: string[];
  already_gone: string[];
  skipped: string[];
  errors: string[];
  cancelled: boolean;
  not_started: number;
}

export interface BuildProgress {
  token: string;
  phase: "find" | "size" | "source" | "remove";
  done: number;
  total: number;
  current: string;
}
