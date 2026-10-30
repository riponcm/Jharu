use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    MlModels,
    DevCache,
    AppData,
}

/// The rating shown on a finding. Dev clean works it out per finding in
/// safety.rs from the rule's facts and live checks; other tabs set it
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    /// Nothing breaks and nothing unique is lost: it comes back by itself.
    Safe,
    /// Nothing unique is lost, but something stops working until a manual
    /// step or a large download, or a program is using it right now.
    Caution,
    /// Holds data that exists nowhere else, or something outside depends on it.
    Risky,
}

/// How a location's contents come back after it is cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// The tool rebuilds or re-downloads what it needs the next time it runs.
    Automatic,
    /// Re-downloaded automatically when next used, but large (models).
    Download,
    /// Only comes back when the user does this ("run `x`", "reinstall y").
    Manual(&'static str),
    /// Never: the contents exist nowhere else. Names what would be lost.
    Never(&'static str),
}

pub struct Rule {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub category: Category,
    pub restore: Restore,
    /// Process names (lowercase, no extension) that mean the location may be
    /// in use right now. Only tools that run while using it, not runtimes
    /// such as node or python that run for every reason.
    pub tools: &'static [&'static str],
    /// A lock file, relative to the location, that the tool holds while it
    /// works there.
    pub lock: Option<&'static str>,
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
    use Restore::*;
    const JETBRAINS: &[&str] = &[
        "idea64", "pycharm64", "clion64", "webstorm64", "rider64", "goland64", "rustrover64",
        "datagrip64", "phpstorm64", "rubymine64", "studio64", "idea", "pycharm", "clion", "webstorm",
        "rider", "goland", "rustrover", "datagrip", "phpstorm", "rubymine",
    ];
    vec![
        Rule { id: "hf-hub", name: "Hugging Face cache", description: "Downloaded models and datasets from the Hugging Face hub. Re-downloaded on demand.", category: MlModels, restore: Download, tools: &[], lock: None },
        Rule { id: "ollama", name: "Ollama models", description: "Local LLM weights pulled with `ollama pull`. Re-downloadable, but large.", category: MlModels, restore: Manual("run `ollama pull <model>`"), tools: &["ollama", "ollama app"], lock: None },
        Rule { id: "torch", name: "PyTorch hub cache", description: "Model checkpoints downloaded via torch.hub and torchvision.", category: MlModels, restore: Download, tools: &[], lock: None },
        Rule { id: "keras", name: "Keras models", description: "Pretrained weights downloaded by Keras applications.", category: MlModels, restore: Download, tools: &[], lock: None },
        Rule { id: "whisper", name: "Whisper models", description: "OpenAI Whisper speech-recognition model weights.", category: MlModels, restore: Download, tools: &[], lock: None },
        Rule { id: "lm-studio", name: "LM Studio models", description: "Models downloaded through LM Studio.", category: MlModels, restore: Manual("download the model again in LM Studio"), tools: &["lm studio", "lms"], lock: None },
        Rule { id: "conda-pkgs", name: "Conda package cache", description: "Downloaded package archives. Safe to clear; `conda clean` does the same.", category: DevCache, restore: Automatic, tools: &["conda", "mamba", "micromamba"], lock: None },
        Rule { id: "conda-envs", name: "Conda environments", description: "Full Python environments. Deleting removes the env — review which ones you still use.", category: AppData, restore: Never("Python environments and the packages installed in them"), tools: &[], lock: None },
        Rule { id: "pip", name: "Pip cache", description: "Downloaded wheels and source archives. Regenerates on next install.", category: DevCache, restore: Automatic, tools: &["pip", "pip3"], lock: None },
        Rule { id: "uv", name: "uv cache", description: "Python package cache used by uv. Regenerates on demand.", category: DevCache, restore: Automatic, tools: &["uv", "uvx"], lock: Some(".lock") },
        Rule { id: "npm", name: "npm cache", description: "Package tarball cache (_cacache). Regenerates on next install.", category: DevCache, restore: Automatic, tools: &[], lock: None },
        Rule { id: "yarn", name: "Yarn cache", description: "Global package cache. Regenerates on next install.", category: DevCache, restore: Automatic, tools: &[], lock: None },
        Rule { id: "pnpm", name: "pnpm store", description: "Content-addressed package store shared across projects. Re-downloaded as needed.", category: DevCache, restore: Automatic, tools: &["pnpm"], lock: None },
        Rule { id: "cargo-registry", name: "Cargo registry", description: "Downloaded Rust crates. Re-fetched on next build.", category: DevCache, restore: Automatic, tools: &["cargo"], lock: Some("../.package-cache") },
        Rule { id: "go-mod", name: "Go module cache", description: "Downloaded Go modules. Re-fetched on next build.", category: DevCache, restore: Automatic, tools: &["go"], lock: None },
        Rule { id: "gradle", name: "Gradle caches", description: "Dependencies and build caches. Re-downloaded on next build.", category: DevCache, restore: Automatic, tools: &[], lock: None },
        Rule { id: "maven", name: "Maven repository", description: "Downloaded Java dependencies (~/.m2). Re-fetched on next build.", category: DevCache, restore: Automatic, tools: &[], lock: None },
        Rule { id: "nuget", name: "NuGet packages", description: "Downloaded .NET packages. Restored on next build.", category: DevCache, restore: Automatic, tools: &[], lock: None },
        Rule { id: "homebrew", name: "Homebrew cache", description: "Downloaded bottle archives. Safe to clear; `brew cleanup` does the same.", category: DevCache, restore: Automatic, tools: &[], lock: None },
        Rule { id: "xcode-derived", name: "Xcode DerivedData", description: "Build intermediates and indexes. Xcode rebuilds them automatically.", category: DevCache, restore: Automatic, tools: &["xcode", "xcodebuild"], lock: None },
        Rule { id: "xcode-archives", name: "Xcode archives", description: "App archives from past releases. Needed for re-symbolication — review first.", category: AppData, restore: Never("archives of shipped builds, needed to read their crash reports"), tools: &[], lock: None },
        Rule { id: "docker", name: "Docker VM data", description: "Docker's virtual disk holding all images, containers, and volumes. Use `docker system prune` instead of deleting.", category: AppData, restore: Never("Docker images, containers and volumes, including any databases kept in volumes"), tools: &["docker desktop", "com.docker.backend", "dockerd"], lock: None },
        Rule { id: "jetbrains", name: "JetBrains caches", description: "IDE indexes and caches. Rebuilt on next launch (first open will be slow).", category: DevCache, restore: Automatic, tools: JETBRAINS, lock: None },
        Rule { id: "playwright", name: "Playwright browsers", description: "Downloaded browser binaries for testing. Re-installed via `npx playwright install`.", category: DevCache, restore: Manual("run `npx playwright install`"), tools: &[], lock: None },
        Rule { id: "puppeteer", name: "Puppeteer browsers", description: "Downloaded Chromium builds. Puppeteer fetches them only on a fresh install.", category: DevCache, restore: Manual("run `npx puppeteer browsers install chrome`"), tools: &[], lock: None },
    ]
}
