use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    MlModels,
    DevCache,
    AppData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    /// Regenerates automatically; safe to clear any time.
    Safe,
    /// Will be re-downloaded/rebuilt on demand — costs bandwidth or time.
    Caution,
    /// May contain state the user still wants; review before cleaning.
    Risky,
}

pub struct Rule {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub category: Category,
    pub safety: Safety,
}

impl Rule {
    /// Candidate locations for this rule on the current OS. Non-existent
    /// paths are filtered out by the scanner.
    pub fn paths(&self) -> Vec<PathBuf> {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return vec![],
        };
        let p = |s: &str| home.join(s);

        match self.id {
            "hf-hub" => vec![p(".cache/huggingface")],
            "ollama" => vec![p(".ollama/models")],
            "torch" => vec![p(".cache/torch")],
            "keras" => vec![p(".keras/models")],
            "whisper" => vec![p(".cache/whisper")],
            "lm-studio" => vec![p(".cache/lm-studio/models"), p(".lmstudio/models")],
            "conda-pkgs" => vec![
                p("miniconda3/pkgs"),
                p("anaconda3/pkgs"),
                p("miniforge3/pkgs"),
                p(".conda/pkgs"),
            ],
            "conda-envs" => vec![
                p("miniconda3/envs"),
                p("anaconda3/envs"),
                p("miniforge3/envs"),
            ],
            "pip" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Caches/pip")];
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("pip/Cache")).unwrap_or_default()];
                #[cfg(all(unix, not(target_os = "macos")))]
                return vec![p(".cache/pip")];
            }
            "uv" => {
                #[cfg(target_os = "macos")]
                return vec![p(".cache/uv"), p("Library/Caches/uv")];
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("uv/cache")).unwrap_or_default()];
                #[cfg(all(unix, not(target_os = "macos")))]
                return vec![p(".cache/uv")];
            }
            "npm" => {
                // npm caches under LocalAppData, not the roaming profile that
                // dirs::data_dir() points at on Windows.
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("npm-cache")).unwrap_or_default()];
                #[cfg(not(target_os = "windows"))]
                return vec![p(".npm/_cacache")];
            }
            "yarn" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Caches/Yarn")];
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("Yarn/Cache")).unwrap_or_default()];
                #[cfg(all(unix, not(target_os = "macos")))]
                return vec![p(".cache/yarn")];
            }
            "pnpm" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/pnpm/store")];
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("pnpm/store")).unwrap_or_default()];
                #[cfg(all(unix, not(target_os = "macos")))]
                return vec![p(".local/share/pnpm/store")];
            }
            "cargo-registry" => vec![p(".cargo/registry")],
            "go-mod" => vec![p("go/pkg/mod")],
            "gradle" => vec![p(".gradle/caches")],
            "maven" => vec![p(".m2/repository")],
            "nuget" => vec![p(".nuget/packages")],
            "homebrew" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Caches/Homebrew")];
                #[cfg(not(target_os = "macos"))]
                return vec![];
            }
            "xcode-derived" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Developer/Xcode/DerivedData")];
                #[cfg(not(target_os = "macos"))]
                return vec![];
            }
            "xcode-archives" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Developer/Xcode/Archives")];
                #[cfg(not(target_os = "macos"))]
                return vec![];
            }
            "docker" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Containers/com.docker.docker/Data/vms")];
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("Docker/wsl")).unwrap_or_default()];
                #[cfg(all(unix, not(target_os = "macos")))]
                return vec![p(".local/share/docker")];
            }
            "jetbrains" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Caches/JetBrains")];
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("JetBrains")).unwrap_or_default()];
                #[cfg(all(unix, not(target_os = "macos")))]
                return vec![p(".cache/JetBrains")];
            }
            "playwright" => {
                #[cfg(target_os = "macos")]
                return vec![p("Library/Caches/ms-playwright")];
                #[cfg(target_os = "windows")]
                return vec![dirs::cache_dir().map(|c| c.join("ms-playwright")).unwrap_or_default()];
                #[cfg(all(unix, not(target_os = "macos")))]
                return vec![p(".cache/ms-playwright")];
            }
            "puppeteer" => vec![p(".cache/puppeteer")],
            _ => vec![],
        }
    }
}

pub fn all_rules() -> Vec<Rule> {
    use Category::*;
    use Safety::*;
    vec![
        Rule { id: "hf-hub", name: "Hugging Face cache", description: "Downloaded models and datasets from the Hugging Face hub. Re-downloaded on demand.", category: MlModels, safety: Caution },
        Rule { id: "ollama", name: "Ollama models", description: "Local LLM weights pulled with `ollama pull`. Re-downloadable, but large.", category: MlModels, safety: Caution },
        Rule { id: "torch", name: "PyTorch hub cache", description: "Model checkpoints downloaded via torch.hub and torchvision.", category: MlModels, safety: Caution },
        Rule { id: "keras", name: "Keras models", description: "Pretrained weights downloaded by Keras applications.", category: MlModels, safety: Caution },
        Rule { id: "whisper", name: "Whisper models", description: "OpenAI Whisper speech-recognition model weights.", category: MlModels, safety: Caution },
        Rule { id: "lm-studio", name: "LM Studio models", description: "Models downloaded through LM Studio.", category: MlModels, safety: Caution },
        Rule { id: "conda-pkgs", name: "Conda package cache", description: "Downloaded package archives. Safe to clear; `conda clean` does the same.", category: DevCache, safety: Safe },
        Rule { id: "conda-envs", name: "Conda environments", description: "Full Python environments. Deleting removes the env — review which ones you still use.", category: AppData, safety: Risky },
        Rule { id: "pip", name: "Pip cache", description: "Downloaded wheels and source archives. Regenerates on next install.", category: DevCache, safety: Safe },
        Rule { id: "uv", name: "uv cache", description: "Python package cache used by uv. Regenerates on demand.", category: DevCache, safety: Safe },
        Rule { id: "npm", name: "npm cache", description: "Package tarball cache (_cacache). Regenerates on next install.", category: DevCache, safety: Safe },
        Rule { id: "yarn", name: "Yarn cache", description: "Global package cache. Regenerates on next install.", category: DevCache, safety: Safe },
        Rule { id: "pnpm", name: "pnpm store", description: "Content-addressed package store shared across projects. Re-downloaded as needed.", category: DevCache, safety: Caution },
        Rule { id: "cargo-registry", name: "Cargo registry", description: "Downloaded Rust crates. Re-fetched on next build.", category: DevCache, safety: Caution },
        Rule { id: "go-mod", name: "Go module cache", description: "Downloaded Go modules. Re-fetched on next build.", category: DevCache, safety: Caution },
        Rule { id: "gradle", name: "Gradle caches", description: "Dependencies and build caches. Re-downloaded on next build.", category: DevCache, safety: Caution },
        Rule { id: "maven", name: "Maven repository", description: "Downloaded Java dependencies (~/.m2). Re-fetched on next build.", category: DevCache, safety: Caution },
        Rule { id: "nuget", name: "NuGet packages", description: "Downloaded .NET packages. Restored on next build.", category: DevCache, safety: Caution },
        Rule { id: "homebrew", name: "Homebrew cache", description: "Downloaded bottle archives. Safe to clear; `brew cleanup` does the same.", category: DevCache, safety: Safe },
        Rule { id: "xcode-derived", name: "Xcode DerivedData", description: "Build intermediates and indexes. Xcode rebuilds them automatically.", category: DevCache, safety: Safe },
        Rule { id: "xcode-archives", name: "Xcode archives", description: "App archives from past releases. Needed for re-symbolication — review first.", category: AppData, safety: Risky },
        Rule { id: "docker", name: "Docker VM data", description: "Docker's virtual disk holding all images, containers, and volumes. Use `docker system prune` instead of deleting.", category: AppData, safety: Risky },
        Rule { id: "jetbrains", name: "JetBrains caches", description: "IDE indexes and caches. Rebuilt on next launch (first open will be slow).", category: DevCache, safety: Safe },
        Rule { id: "playwright", name: "Playwright browsers", description: "Downloaded browser binaries for testing. Re-installed via `npx playwright install`.", category: DevCache, safety: Safe },
        Rule { id: "puppeteer", name: "Puppeteer browsers", description: "Downloaded Chromium builds. Re-installed on next `npm install`.", category: DevCache, safety: Safe },
    ]
}
