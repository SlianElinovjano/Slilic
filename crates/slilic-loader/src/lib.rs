//! 把外部 Rust 源码在**玩家本机**编译成动态库，并加载进游戏进程。
//!
//! # 定位
//!
//! slilic 不是"给 mod 作者用的构建工具"，而是一套**脚本运行时**：
//!
//! - **mod 作者**发布 `.rs` 源码，不发布二进制；
//! - **玩家**的机器上完成编译，产物只服务本机；
//! - 编译结果按内容寻址缓存，源码与工具链不变就永远不再编译。
//!
//! 和 JIT / 热重载的区别在于：编译一次、之后一直跑机器码，追求运行期零解释
//! 开销，而不是编辑期的秒级反馈。游戏循环里没有任何编译器在场。
//!
//! # 游戏集成方需要准备什么
//!
//! 1. **随游戏分发一套 rustc。** 玩家机器上大概率没有 Rust 工具链，所以要么
//!    内置一份，要么在启动器里引导安装。把 `RUSTC` 环境变量指向内置的
//!    `rustc` 即可，[`Toolchain::detect`] 会自动采用它。
//! 2. **在安装或首次启动时调 [`ModHost::precompile`]**，把所有 mod 一次编完，
//!    配上进度界面。之后每次启动只用 [`ModHost::load`]，成本是一次动态库加载。
//! 3. **容忍单个 mod 编译失败。** [`ModHost::precompile`] 收集失败项而不中断
//!    整批，宿主应把失败的 mod 标记为不可用，而不是阻止游戏启动。
//!
//! # 缓存键包含什么
//!
//! 源码内容、crate 名、[`slilic_api::MOD_ABI_VERSION`]、rustc 工具链指纹、
//! 目标平台、优化级别。键里**不含任何路径信息**，所以玩家把游戏挪到别的盘、
//! 换个安装目录，缓存依然命中。
//!
//! # 典型用法
//!
//! ```no_run
//! # use std::path::Path;
//! # use slilic_loader::{ModHost, ModSource};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let host = ModHost::new(".mod-cache", "crates/slilic-api/src/lib.rs", 2)?;
//!
//! // 安装 / 首次启动：批量编译，单个失败不影响其余
//! let report = host.precompile(&[ModSource::new("example_mod", "example-mod")])?;
//! println!(
//!     "编译 {} 个，命中缓存 {} 个，失败 {} 个",
//!     report.compiled,
//!     report.cache_hits,
//!     report.failures.len()
//! );
//!
//! // 之后每次启动：只加载，不再编译
//! let (module, _) = host.load(Path::new("example-mod"), "example_mod")?;
//! module.init()?;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

mod cache;
mod compile;
pub mod error;
mod load;
mod manager;

use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use slilic_api::MOD_ABI_VERSION;

use crate::error::IoResultExt;

pub use cache::{CacheEntry, CacheInputs, CacheKey, CompileCache, compute_key};
pub use compile::{CompileRequest, Toolchain};
pub use error::LoaderError;
pub use load::LoadedMod;
pub use manager::{ModManager, ModOutcome};

/// 编译产物在各平台上的文件名。
///
/// 统一叫 `mod`，让缓存目录的结构与平台无关。
#[must_use]
pub fn dylib_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "mod.dll"
    } else if cfg!(target_os = "macos") {
        "mod.dylib"
    } else {
        "mod.so"
    }
}

/// 一次加载是命中缓存还是真的编译了。
#[derive(Debug, Clone)]
pub enum BuildReport {
    /// 缓存命中：没有调用编译器。
    CacheHit {
        /// 缓存键。
        key: String,
        /// 复用的动态库路径。
        artifact: PathBuf,
        /// 仅加载动态库所花的时间。
        elapsed: Duration,
    },
    /// 缓存未命中：这次真的调了 rustc。
    Compiled {
        /// 缓存键。
        key: String,
        /// 新产出的动态库路径。
        artifact: PathBuf,
        /// 编译加落盘所花的时间。
        elapsed: Duration,
    },
}

impl BuildReport {
    /// 本次操作的耗时。缓存命中时是加载时间，未命中时是编译时间。
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        match self {
            Self::CacheHit { elapsed, .. } | Self::Compiled { elapsed, .. } => *elapsed,
        }
    }

    /// 缓存键。
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::CacheHit { key, .. } | Self::Compiled { key, .. } => key,
        }
    }

    /// 动态库路径。
    #[must_use]
    pub fn artifact(&self) -> &Path {
        match self {
            Self::CacheHit { artifact, .. } | Self::Compiled { artifact, .. } => artifact,
        }
    }

    /// 是否直接复用了已有产物。
    #[must_use]
    pub fn is_cache_hit(&self) -> bool {
        matches!(self, Self::CacheHit { .. })
    }
}

/// 一个待编译的 mod。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSource {
    /// crate 名，用于区分不同 mod 的产物目录。
    pub crate_name: String,
    /// mod 源码根目录，入口约定为 `src/lib.rs`。
    pub source_root: PathBuf,
}

impl ModSource {
    /// 构造一个待编译的 mod。
    pub fn new(crate_name: impl Into<String>, source_root: impl Into<PathBuf>) -> Self {
        Self {
            crate_name: crate_name.into(),
            source_root: source_root.into(),
        }
    }
}

/// 单个 mod 预编译失败的原因。
#[derive(Debug)]
pub struct ModFailure {
    /// 出问题的 mod。
    pub source: ModSource,
    /// 失败原因。编译错误时携带 rustc 的原始 stderr。
    pub error: LoaderError,
}

/// 批量预编译的结果。
///
/// 这是给"首次启动界面"用的数据结构：宿主可以把 `compiled + cache_hits`
/// 当作进度总量，把 `failures` 渲染成警告列表。
#[derive(Debug, Default)]
pub struct PrecompileReport {
    /// 实际调用了 rustc 的 mod 数。
    pub compiled: usize,
    /// 直接命中缓存、没有调用编译器的 mod 数。
    pub cache_hits: usize,
    /// 失败的 mod。**任意一项失败都不影响其余 mod 继续编译**。
    pub failures: Vec<ModFailure>,
    /// 整批耗时。
    pub elapsed: Duration,
}

impl PrecompileReport {
    /// 成功的 mod 数（编译成功 + 命中缓存）。
    #[must_use]
    pub fn succeeded(&self) -> usize {
        self.compiled + self.cache_hits
    }

    /// 是否全部成功。
    #[must_use]
    pub fn all_succeeded(&self) -> bool {
        self.failures.is_empty()
    }
}

/// 宿主侧的统一入口：管理工具链、缓存和 `slilic-api` 的 rlib。
///
/// 用 [`ModHost::builder`] 做完整配置；[`ModHost::new`] 是常用默认值的快捷方式。
pub struct ModHost {
    toolchain: Toolchain,
    cache: CompileCache,
    api_rlib: PathBuf,
    opt_level: u8,
    /// 并行编译上限；0 表示"按机器自动决定"。
    ///
    /// 存成原子量是为了让游戏作者能在运行期调整——比如把设置界面里的
    /// "编译并发数"直接透给玩家。
    max_parallel: AtomicUsize,
}

impl ModHost {
    /// 建立宿主：探测工具链，必要时先把 `slilic-api` 编成 rlib。
    ///
    /// `api_src` 是 `slilic-api` 的 `lib.rs` 路径。它编出的 rlib 会按工具链
    /// 指纹分目录缓存在缓存根目录下，因此换 rustc 版本会自动重新生成。
    ///
    /// 这是 [`ModHost::builder`] 的快捷方式：优化级别取传入值，并发数按机器自动决定。
    ///
    /// # Errors
    ///
    /// 工具链不可用、缓存目录不可写或 `slilic-api` 编译失败时返回错误。
    pub fn new(
        cache_root: impl AsRef<Path>,
        api_src: impl AsRef<Path>,
        opt_level: u8,
    ) -> Result<Self, LoaderError> {
        Self::builder()
            .cache_dir(cache_root)
            .api_source(api_src)
            .opt_level(opt_level)
            .build()
    }

    /// 开始配置一个宿主。
    #[must_use]
    pub fn builder() -> ModHostBuilder {
        ModHostBuilder::default()
    }

    /// 当前生效的并行编译上限。
    ///
    /// 未显式设置时取机器的可用并行度，并夹在 `1..=8` 之间——并行跑 rustc
    /// 每个进程要几百 MB 内存，核多不等于应该开满。
    #[must_use]
    pub fn max_parallel(&self) -> usize {
        let configured = self.max_parallel.load(Ordering::Relaxed);
        if configured == 0 {
            default_parallelism()
        } else {
            configured
        }
    }

    /// 运行期调整并行编译上限。
    ///
    /// 传 0 表示恢复"按机器自动决定"。游戏作者可以把它直接接到玩家可见的
    /// 设置项上。
    pub fn set_max_parallel(&self, workers: usize) {
        self.max_parallel.store(workers, Ordering::Relaxed);
    }

    /// 只读访问缓存，用于 `inspect` / `clean`。
    #[must_use]
    pub fn cache(&self) -> &CompileCache {
        &self.cache
    }

    /// 只读访问工具链信息。
    #[must_use]
    pub fn toolchain(&self) -> &Toolchain {
        &self.toolchain
    }

    /// 当前的优化级别。
    #[must_use]
    pub fn opt_level(&self) -> u8 {
        self.opt_level
    }

    /// 只保证 mod 的机器码产物存在，不加载进进程。
    ///
    /// 这是"玩家本地编译"的原子操作：命中缓存立即返回，未命中才调 rustc。
    /// 适合在安装阶段批量调用。
    ///
    /// # Errors
    ///
    /// 源码不可读、缺少入口文件或编译失败时返回错误。
    pub fn build(&self, source_root: &Path, crate_name: &str) -> Result<BuildReport, LoaderError> {
        let outcome = self.ensure_artifact(source_root, crate_name)?;
        Ok(if outcome.from_cache {
            BuildReport::CacheHit {
                key: outcome.key,
                artifact: outcome.artifact,
                elapsed: Duration::ZERO,
            }
        } else {
            BuildReport::Compiled {
                key: outcome.key,
                artifact: outcome.artifact,
                elapsed: outcome.elapsed,
            }
        })
    }

    /// 批量预编译，给"安装 / 首次启动"用。
    ///
    /// 编译在多个工作线程上并行推进，并发上限取 [`ModHost::max_parallel`]。
    /// 缓存命中的 mod 几乎不耗时，所以同一个函数既能做首次全量编译，也能做
    /// 后续的"确认一下都还在"。
    ///
    /// 刻意**不因为单个 mod 失败而中断整批**：失败项收集进
    /// [`PrecompileReport::failures`]，其余 mod 照常编译。工作线程即使 panic
    /// 也会被就地隔离成一条失败记录，不影响其他 mod。这样玩家机器上某个 mod
    /// 编译不过时，游戏仍能正常启动，只是那个 mod 不可用。
    ///
    /// 返回的报告按入参顺序排列，方便游戏作者把进度和界面列表对上。
    ///
    /// # Errors
    ///
    /// 目前只有宿主自身状态异常时才返回 `Err`；单个 mod 的问题一律走
    /// `failures`，不会向上冒泡。
    pub fn precompile(&self, sources: &[ModSource]) -> Result<PrecompileReport, LoaderError> {
        let started = Instant::now();
        let workers = self.max_parallel().clamp(1, sources.len().max(1));
        let cursor = AtomicUsize::new(0);

        let mut results: Vec<(usize, Result<ArtifactOutcome, LoaderError>)> =
            Vec::with_capacity(sources.len());

        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for _ in 0..workers {
                handles.push(scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let index = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(source) = sources.get(index) else {
                            break;
                        };
                        // 单个任务 panic 就地隔离，不让它逃出工作线程。
                        let outcome = catch_unwind(AssertUnwindSafe(|| {
                            self.ensure_artifact(&source.source_root, &source.crate_name)
                        }))
                        .unwrap_or_else(|_| {
                            Err(LoaderError::CompileThreadPanicked {
                                crate_name: source.crate_name.clone(),
                            })
                        });
                        local.push((index, outcome));
                    }
                    local
                }));
            }

            for handle in handles {
                // 任务级 panic 已由上面的 catch_unwind 兜住；走到这里说明是
                // 线程自身出了问题，宁可丢弃该线程的结果也不让整批崩掉。
                if let Ok(local) = handle.join() {
                    results.extend(local);
                }
            }
        });

        results.sort_by_key(|(index, _)| *index);

        let mut report = PrecompileReport::default();
        for (index, outcome) in results {
            match outcome {
                Ok(artifact) => {
                    if artifact.from_cache {
                        report.cache_hits += 1;
                    } else {
                        report.compiled += 1;
                    }
                }
                Err(error) => {
                    if let Some(source) = sources.get(index) {
                        report.failures.push(ModFailure {
                            source: source.clone(),
                            error,
                        });
                    }
                }
            }
        }

        report.elapsed = started.elapsed();
        Ok(report)
    }

    /// 加载一个 mod：命中缓存则直接加载，否则先编译再加载。
    ///
    /// `source_root` 是 mod 源码根目录，入口约定为 `src/lib.rs`（也接受
    /// 直接放在根目录的 `lib.rs`）。`crate_name` 用于区分不同的 mod。
    ///
    /// # Errors
    ///
    /// 源码不可读、编译失败、ABI 不匹配或动态库无法加载时返回错误。
    pub fn load(
        &self,
        source_root: &Path,
        crate_name: &str,
    ) -> Result<(LoadedMod, BuildReport), LoaderError> {
        let outcome = self.ensure_artifact(source_root, crate_name)?;

        let started = Instant::now();
        let module = load::load(&outcome.artifact)?;

        let report = if outcome.from_cache {
            BuildReport::CacheHit {
                key: outcome.key,
                artifact: outcome.artifact,
                elapsed: started.elapsed(),
            }
        } else {
            BuildReport::Compiled {
                key: outcome.key,
                artifact: outcome.artifact,
                elapsed: outcome.elapsed,
            }
        };

        Ok((module, report))
    }

    /// 算缓存键、查缓存、必要时编译——"确保产物可用"的完整流程。
    fn ensure_artifact(
        &self,
        source_root: &Path,
        crate_name: &str,
    ) -> Result<ArtifactOutcome, LoaderError> {
        let key = compute_key(&CacheInputs {
            source_root,
            crate_name,
            abi_version: MOD_ABI_VERSION,
            toolchain_fingerprint: self.toolchain.fingerprint(),
            target_triple: self.toolchain.host_triple(),
            opt_level: self.opt_level,
        })?;

        if let Some(artifact) = self.cache.lookup(crate_name, &key, dylib_file_name()) {
            return Ok(ArtifactOutcome {
                key: key.as_str().to_owned(),
                artifact,
                from_cache: true,
                elapsed: Duration::ZERO,
            });
        }

        let started = Instant::now();
        let artifact = self.compile_into_cache(source_root, crate_name, &key)?;

        Ok(ArtifactOutcome {
            key: key.as_str().to_owned(),
            artifact,
            from_cache: false,
            elapsed: started.elapsed(),
        })
    }

    /// 编译到暂存目录，成功后整体改名进缓存，避免留下半成品。
    fn compile_into_cache(
        &self,
        source_root: &Path,
        crate_name: &str,
        key: &CacheKey,
    ) -> Result<PathBuf, LoaderError> {
        let entry = resolve_entry(source_root)?;
        let final_dir = self.cache.artifact_dir(crate_name, key);
        let staging =
            final_dir.with_file_name(format!("{crate_name}-{key}.staging-{}", std::process::id()));

        if staging.is_dir() {
            fs::remove_dir_all(&staging).at(&staging)?;
        }
        fs::create_dir_all(&staging).at(&staging)?;

        let output = staging.join(dylib_file_name());
        let result = self.toolchain.compile_mod(&CompileRequest {
            source: &entry,
            api_rlib: &self.api_rlib,
            output: &output,
            crate_name,
            opt_level: self.opt_level,
        });

        if let Err(err) = result {
            // 编译失败时清掉暂存目录。清理失败不掩盖原始错误，所以只做记录。
            let _cleanup = fs::remove_dir_all(&staging);
            return Err(err);
        }

        if final_dir.is_dir() {
            fs::remove_dir_all(&final_dir).at(&final_dir)?;
        }
        fs::rename(&staging, &final_dir).at(&final_dir)?;

        Ok(final_dir.join(dylib_file_name()))
    }
}

/// 默认并发度：取机器可用并行度，夹在 `1..=8`。
///
/// 并行跑 rustc 每个进程要吃几百 MB 内存，核多不等于应该开满。
fn default_parallelism() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, 8)
}

/// [`ModHost`] 的配置器。
///
/// 所有项都有合理默认值，只有缓存目录和 `slilic-api` 源码路径是必需的。
///
/// ```no_run
/// use slilic_loader::ModHost;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let host = ModHost::builder()
///     .cache_dir(".mod-cache")
///     .api_source("crates/slilic-api/src/lib.rs")
///     .opt_level(3)
///     .max_parallel(4) // 游戏作者可以直接把它透给玩家的设置界面
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default)]
pub struct ModHostBuilder {
    cache_root: Option<PathBuf>,
    api_src: Option<PathBuf>,
    opt_level: Option<u8>,
    max_parallel: Option<usize>,
}

impl ModHostBuilder {
    /// 编译产物缓存的根目录。**必需。**
    #[must_use]
    pub fn cache_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.cache_root = Some(path.as_ref().to_path_buf());
        self
    }

    /// `slilic-api` 的 `lib.rs` 路径。**必需。**
    #[must_use]
    pub fn api_source(mut self, path: impl AsRef<Path>) -> Self {
        self.api_src = Some(path.as_ref().to_path_buf());
        self
    }

    /// 优化级别，默认 2。
    #[must_use]
    pub fn opt_level(mut self, level: u8) -> Self {
        self.opt_level = Some(level);
        self
    }

    /// 并行编译的上限。
    ///
    /// 不设置时按机器自动决定（可用并行度夹在 `1..=8`）。运行期还可以用
    /// [`ModHost::set_max_parallel`] 再改。
    #[must_use]
    pub fn max_parallel(mut self, workers: usize) -> Self {
        self.max_parallel = Some(workers);
        self
    }

    /// 完成配置、探测工具链，必要时预先把 `slilic-api` 编成 rlib。
    ///
    /// # Errors
    ///
    /// 缺少必需项时返回 [`LoaderError::Config`]；工具链不可用、缓存目录不可写
    /// 或 `slilic-api` 编译失败时返回对应错误。
    pub fn build(self) -> Result<ModHost, LoaderError> {
        let cache_root = self.cache_root.ok_or_else(|| LoaderError::Config {
            message: "缺少缓存目录，请先调用 cache_dir()".to_owned(),
        })?;
        let api_src = self.api_src.ok_or_else(|| LoaderError::Config {
            message: "缺少 slilic-api 源码路径，请先调用 api_source()".to_owned(),
        })?;

        let toolchain = Toolchain::detect()?;
        let cache = CompileCache::new(&cache_root);

        let api_dir = cache
            .root()
            .join("_toolchain")
            .join(toolchain.fingerprint());
        let api_rlib = api_dir.join("libslilic_api.rlib");
        if !api_rlib.is_file() {
            fs::create_dir_all(&api_dir).at(&api_dir)?;
            toolchain.build_api_rlib(&api_src, &api_rlib)?;
        }

        Ok(ModHost {
            toolchain,
            cache,
            api_rlib,
            opt_level: self.opt_level.unwrap_or(2),
            max_parallel: AtomicUsize::new(self.max_parallel.unwrap_or(0)),
        })
    }
}

/// "确保产物存在"的结果。
struct ArtifactOutcome {
    /// 缓存键。
    key: String,
    /// 动态库路径。
    artifact: PathBuf,
    /// `true` 表示直接命中缓存，没有调用编译器。
    from_cache: bool,
    /// 编译耗时；命中缓存时为 0。
    elapsed: Duration,
}

/// 定位 mod 的入口源文件。
fn resolve_entry(source_root: &Path) -> Result<PathBuf, LoaderError> {
    let candidates = [
        source_root.join("src").join("lib.rs"),
        source_root.join("lib.rs"),
    ];
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| LoaderError::Io {
            path: source_root.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "mod 缺少 src/lib.rs 或 lib.rs",
            ),
        })
}
