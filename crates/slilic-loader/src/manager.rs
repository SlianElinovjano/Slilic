//! 多 mod 的加载调度。
//!
//! 编译和加载是两件性质完全不同的事，节奏也不一样：
//!
//! | | 编译 | 加载 |
//! |---|---|---|
//! | 耗时 | 每个 mod 一秒上下 | 毫秒级 |
//! | 能否并行 | 能，且应该并行 | 不能，必须串行 |
//! | 线程要求 | 随便哪个线程 | 通常要求主线程 |
//!
//! [`ModManager`] 把这两件事摊开交给游戏作者，由作者决定什么时候做什么。
//!
//! # 两种典型节奏
//!
//! **一次性**——mod 数量少，或者已经预编译过：
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use slilic_loader::{ModHost, ModManager, ModSource};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let host = Arc::new(ModHost::new(".mod-cache", "crates/slilic-api/src/lib.rs", 2)?);
//! let mut manager = ModManager::new(Arc::clone(&host));
//! manager.enqueue_all([ModSource::new("example_mod", "example-mod")]);
//!
//! for outcome in manager.finish() {
//!     println!("{} -> {}", outcome.source.crate_name, outcome.is_ok());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! **分帧推进**——想把加载摊到若干帧里，顺便让进度条动起来：
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use slilic_loader::{ModHost, ModManager, ModSource};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let host = Arc::new(ModHost::new(".mod-cache", "crates/slilic-api/src/lib.rs", 2)?);
//! # let mut manager = ModManager::new(Arc::clone(&host));
//! # manager.enqueue_all([ModSource::new("example_mod", "example-mod")]);
//! // 每帧推进一个
//! while let Some(outcome) = manager.advance() {
//!     let (done, total) = manager.progress();
//!     println!("{done}/{total}  {}", outcome.source.crate_name);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # 分帧推进的前提
//!
//! [`ModManager::advance`] 如果发现目标 mod 还没编译过，会**当场阻塞地触发编译**，
//! 一帧卡一秒。所以真正想要平滑的分帧体验，应该先用
//! [`ModHost::precompile`] 把编译并行做完，再用 `advance` 逐步加载。

use std::collections::VecDeque;
use std::sync::Arc;

use crate::{BuildReport, LoadedMod, ModFailure, ModHost, ModSource};

/// 一次推进的结果。
#[derive(Debug)]
pub struct ModOutcome {
    /// 本次处理的 mod。
    pub source: ModSource,
    /// 成功时的加载报告；失败时为 `None`。
    pub report: Option<BuildReport>,
    /// 失败时的可读错误描述；成功时为 `None`。
    ///
    /// 需要完整错误（含 rustc 原始 stderr）请查 [`ModManager::failures`]。
    pub error: Option<String>,
}

impl ModOutcome {
    /// 本次是否加载成功。
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.report.is_some()
    }

    /// 本次是否失败。
    #[must_use]
    pub fn is_err(&self) -> bool {
        self.error.is_some()
    }
}

/// 多 mod 的加载管理器，让游戏作者自己控制加载节奏。
///
/// 持有 [`ModHost`] 的共享引用，所以可以和其他组件共用同一个宿主。
/// 管理器本身**不是线程安全的**——它是给主线程的游戏循环用的。
pub struct ModManager {
    host: Arc<ModHost>,
    queue: VecDeque<ModSource>,
    loaded: Vec<LoadedMod>,
    failures: Vec<ModFailure>,
    processed: usize,
}

impl ModManager {
    /// 用共享的宿主建立管理器。
    #[must_use]
    pub fn new(host: Arc<ModHost>) -> Self {
        Self {
            host,
            queue: VecDeque::new(),
            loaded: Vec::new(),
            failures: Vec::new(),
            processed: 0,
        }
    }

    /// 追加一个待加载的 mod。
    pub fn enqueue(&mut self, source: ModSource) {
        self.queue.push_back(source);
    }

    /// 追加一批待加载的 mod。
    pub fn enqueue_all<I>(&mut self, sources: I)
    where
        I: IntoIterator<Item = ModSource>,
    {
        self.queue.extend(sources);
    }

    /// 还没处理的 mod 数量。
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// 已处理的 mod 数量（成功 + 失败）。
    #[must_use]
    pub fn processed(&self) -> usize {
        self.processed
    }

    /// 总进度 `(已完成, 总数)`，可以直接喂给进度条。
    #[must_use]
    pub fn progress(&self) -> (usize, usize) {
        (self.processed, self.processed + self.queue.len())
    }

    /// 推进一个 mod，返回 `None` 表示队列已空。
    ///
    /// 成功时 mod 的所有权移交到 [`ModManager::loaded`]，失败时记录进
    /// [`ModManager::failures`]。无论成败都不会 panic，也不会中断队列。
    #[must_use = "advance 返回 None 表示队列已空，直接丢弃通常说明循环写错了"]
    pub fn advance(&mut self) -> Option<ModOutcome> {
        let source = self.queue.pop_front()?;
        self.processed += 1;

        match self.host.load(&source.source_root, &source.crate_name) {
            Ok((module, report)) => {
                self.loaded.push(module);
                Some(ModOutcome {
                    source,
                    report: Some(report),
                    error: None,
                })
            }
            Err(error) => {
                let message = error.to_string();
                self.failures.push(ModFailure {
                    source: source.clone(),
                    error,
                });
                Some(ModOutcome {
                    source,
                    report: None,
                    error: Some(message),
                })
            }
        }
    }

    /// 一次最多推进 `budget` 个 mod，返回实际处理数量。
    ///
    /// 适合"这一帧最多加载 2 个"这类限速需求。
    pub fn advance_by(&mut self, budget: usize) -> usize {
        let mut done = 0;
        while done < budget && self.advance().is_some() {
            done += 1;
        }
        done
    }

    /// 把队列里剩下的 mod 全部处理完。
    pub fn finish(&mut self) -> Vec<ModOutcome> {
        let mut outcomes = Vec::with_capacity(self.queue.len());
        while let Some(outcome) = self.advance() {
            outcomes.push(outcome);
        }
        outcomes
    }

    /// 已成功加载的 mod。
    #[must_use]
    pub fn loaded(&self) -> &[LoadedMod] {
        &self.loaded
    }

    /// 加载失败的 mod，含完整错误信息。
    #[must_use]
    pub fn failures(&self) -> &[ModFailure] {
        &self.failures
    }

    /// 取出已加载的 mod，交给游戏自己的注册表或 ECS。
    ///
    /// 取出之后管理器不再持有它们；动态库的生命周期随之转移。
    #[must_use]
    pub fn take_loaded(&mut self) -> Vec<LoadedMod> {
        std::mem::take(&mut self.loaded)
    }

    /// 取底层的宿主，用于按需编译或查询缓存。
    #[must_use]
    pub fn host(&self) -> &ModHost {
        &self.host
    }
}
