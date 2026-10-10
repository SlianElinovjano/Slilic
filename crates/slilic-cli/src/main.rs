//! 演示宿主：编译、加载并驱动外部 Rust mod。
//!
//! ```text
//! slilic precompile <mod目录>... [--jobs N]  并行预编译（安装/首次启动用）
//! slilic load <mod目录>...                   用加载管理器逐个加载
//! slilic run <mod目录> [帧数]                加载并运行 mod
//! slilic build <mod目录>                     只编译，不加载
//! slilic inspect                             查看缓存内容
//! slilic clean                               清空缓存
//! ```
//!
//! 想验证缓存效果，连着跑两次 `run` 或 `build` 就行：第一次是
//! `Compiled`，第二次是 `CacheHit`。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use slilic_api::ModTickContext;
use slilic_loader::{BuildReport, LoaderError, ModHost, ModManager, ModSource};

/// 演示用的优化级别。
const OPT_LEVEL: u8 = 2;
/// 默认运行的帧数。
const DEFAULT_TICKS: u64 = 32;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("\n{err}");
            // 批量操作的部分失败已经逐条打印过明细，不必再刷一遍用法。
            if !matches!(err, RunError::PartialFailure(_)) {
                eprintln!("\n用法:");
                eprintln!("  slilic precompile <mod目录>... [--jobs N]  并行预编译");
                eprintln!("  slilic load <mod目录>...                   逐个加载");
                eprintln!("  slilic run <mod目录> [帧数]                加载并运行");
                eprintln!("  slilic build <mod目录>                     只编译");
                eprintln!("  slilic inspect                             查看缓存");
                eprintln!("  slilic clean                               清空缓存");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), RunError> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_owned());

    match command.as_str() {
        "precompile" => {
            let (dirs, jobs) = parse_precompile_args(args.collect())?;
            precompile_mods(&dirs, jobs)
        }
        "load" => {
            let dirs: Vec<String> = args.collect();
            load_mods(&dirs)
        }
        "run" => {
            let dir = args
                .next()
                .ok_or_else(|| RunError::Usage("run 需要指定 mod 目录".to_owned()))?;
            let ticks = parse_ticks(args.next())?;
            run_mod(&PathBuf::from(dir), ticks)
        }
        "build" => {
            let dir = args
                .next()
                .ok_or_else(|| RunError::Usage("build 需要指定 mod 目录".to_owned()))?;
            build_mod(&PathBuf::from(dir))
        }
        "inspect" => inspect(),
        "clean" => clean(),
        _ => Err(RunError::Usage(format!("未知子命令: {command}"))),
    }
}

/// 解析 `precompile` 的参数，把 `--jobs N` 摘出来。
fn parse_precompile_args(args: Vec<String>) -> Result<(Vec<String>, Option<usize>), RunError> {
    let mut dirs = Vec::new();
    let mut jobs = None;
    let mut iter = args.into_iter();

    while let Some(item) = iter.next() {
        if item == "--jobs" || item == "-j" {
            let raw = iter
                .next()
                .ok_or_else(|| RunError::Usage("--jobs 后面需要跟一个数字".to_owned()))?;
            let parsed = raw
                .parse::<usize>()
                .map_err(|_| RunError::Usage(format!("--jobs 不是合法数字: {raw}")))?;
            jobs = Some(parsed);
        } else {
            dirs.push(item);
        }
    }

    Ok((dirs, jobs))
}

/// 并行预编译。模拟"玩家首次启动时把所有 mod 一次编完"。
fn precompile_mods(dirs: &[String], jobs: Option<usize>) -> Result<(), RunError> {
    if dirs.is_empty() {
        return Err(RunError::Usage(
            "precompile 至少需要一个 mod 目录".to_owned(),
        ));
    }

    let host = make_host()?;
    // 运行期调整并发上限——游戏作者可以把它接到玩家可见的设置项上。
    if let Some(jobs) = jobs {
        host.set_max_parallel(jobs);
    }

    let root = workspace_root();
    let sources: Vec<ModSource> = dirs
        .iter()
        .map(|dir| {
            let relative = Path::new(dir);
            ModSource::new(crate_name_of(relative), root.join(relative))
        })
        .collect();

    println!(
        "批量预编译 {} 个 mod（rustc {}，并发上限 {}）...",
        sources.len(),
        host.toolchain().release(),
        host.max_parallel()
    );
    println!();

    let report = host.precompile(&sources)?;

    println!("  实际编译    {} 个", report.compiled);
    println!("  命中缓存    {} 个", report.cache_hits);
    println!("  失败        {} 个", report.failures.len());
    println!("  总耗时      {:.3} 秒", report.elapsed.as_secs_f64());

    if report.failures.is_empty() {
        println!();
        println!("全部就绪。之后启动游戏只会做动态库加载。");
        return Ok(());
    }

    println!();
    println!("失败的 mod（宿主应把它们的玩法禁用掉，而不是阻止游戏启动）:");
    for failure in &report.failures {
        println!();
        println!(
            "  {} <- {}",
            failure.source.crate_name,
            failure.source.source_root.display()
        );
        for line in failure.error.to_string().lines() {
            println!("      {line}");
        }
    }

    Err(RunError::PartialFailure(report.failures.len()))
}

/// 用加载管理器逐个加载，演示"由游戏自己控制加载时机"。
fn load_mods(dirs: &[String]) -> Result<(), RunError> {
    if dirs.is_empty() {
        return Err(RunError::Usage("load 至少需要一个 mod 目录".to_owned()));
    }

    let host = Arc::new(make_host()?);
    let root = workspace_root();

    let mut manager = ModManager::new(Arc::clone(&host));
    manager.enqueue_all(dirs.iter().map(|dir| {
        let relative = Path::new(dir);
        ModSource::new(crate_name_of(relative), root.join(relative))
    }));

    println!(
        "待加载 {} 个 mod，每个 advance() 处理一个",
        manager.pending()
    );
    println!();

    while let Some(outcome) = manager.advance() {
        let (done, total) = manager.progress();
        if let Some(report) = &outcome.report {
            let how = if report.is_cache_hit() {
                "缓存命中"
            } else {
                "已编译"
            };
            println!("  [{done}/{total}] {}  {how}", outcome.source.crate_name);
        } else if let Some(error) = &outcome.error {
            println!(
                "  [{done}/{total}] {}  失败: {error}",
                outcome.source.crate_name
            );
        }
    }

    println!();
    println!(
        "加载完成：成功 {} 个，失败 {} 个",
        manager.loaded().len(),
        manager.failures().len()
    );

    for module in manager.take_loaded() {
        module.init()?;
        module.shutdown()?;
    }
    println!("已逐个初始化并卸载。");
    Ok(())
}

/// 加载并驱动 mod。
fn run_mod(mod_dir: &Path, ticks: u64) -> Result<(), RunError> {
    let host = make_host()?;
    let crate_name = crate_name_of(mod_dir);
    let directory = workspace_root().join(mod_dir);

    println!("mod 目录    {}", directory.display());
    println!("crate 名    {crate_name}");
    println!(
        "工具链      rustc {} ({})",
        host.toolchain().release(),
        host.toolchain().host_triple()
    );
    println!("优化级别    {}", host.opt_level());
    println!();

    let (module, report) = host.load(&directory, &crate_name)?;
    print_report(&report);
    println!();

    module.init()?;

    let started = Instant::now();
    for tick in 0..ticks {
        let context = ModTickContext {
            tick,
            delta_secs: 1.0 / 60.0,
            entity_count: 1200 + u32::try_from(tick % 40).unwrap_or_default() * 7,
        };
        module.tick(&context)?;
    }
    let elapsed = started.elapsed();

    module.shutdown()?;

    println!();
    println!(
        "运行完成    {ticks} 帧，耗时 {:.3} ms（{:.1} 帧/秒）",
        elapsed.as_secs_f64() * 1000.0,
        ticks as f64 / elapsed.as_secs_f64().max(f64::EPSILON)
    );

    if !report.is_cache_hit() {
        println!("提示        再执行一次同样的命令，即可看到缓存命中。");
    }
    Ok(())
}

/// 只编译，不加载。
fn build_mod(mod_dir: &Path) -> Result<(), RunError> {
    let host = make_host()?;
    let crate_name = crate_name_of(mod_dir);
    let directory = workspace_root().join(mod_dir);

    let started = Instant::now();
    let report = host.build(&directory, &crate_name)?;

    print_report(&report);
    println!("缓存根目录  {}", host.cache().root().display());
    println!("总耗时      {:.3} 秒", started.elapsed().as_secs_f64());
    Ok(())
}

/// 列出缓存内容。
fn inspect() -> Result<(), RunError> {
    let host = make_host()?;
    let entries = host.cache().entries()?;

    println!("缓存根目录  {}", host.cache().root().display());
    println!("记录数      {}", entries.len());
    println!();

    if entries.is_empty() {
        println!("（空）先跑一次 `slilic build example-mod`。");
        return Ok(());
    }

    let mut total = 0_u64;
    for entry in &entries {
        let name = entry
            .dir
            .file_name()
            .map_or_else(|| "?".to_owned(), |n| n.to_string_lossy().into_owned());
        total += entry.bytes;
        println!(
            "  {name:<40}  {:>10}  {}",
            human_bytes(entry.bytes),
            human_age(entry.modified)
        );
    }
    println!();
    println!("合计        {}", human_bytes(total));
    Ok(())
}

/// 清空缓存。
fn clean() -> Result<(), RunError> {
    let host = make_host()?;
    let root = host.cache().root().to_path_buf();
    host.cache().clear()?;
    println!("已清空 {}", root.display());
    Ok(())
}

/// 建立宿主。缓存与 `slilic-api` 源码都相对于 workspace 根目录定位。
fn make_host() -> Result<ModHost, LoaderError> {
    let root = workspace_root();
    ModHost::builder()
        .cache_dir(root.join(".mod-cache"))
        .api_source(
            root.join("crates")
                .join("slilic-api")
                .join("src")
                .join("lib.rs"),
        )
        .opt_level(OPT_LEVEL)
        .build()
}

/// 无论从哪个目录启动，都定位到 workspace 根。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// 用目录名推导 crate 名。
fn crate_name_of(dir: &Path) -> String {
    dir.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mod")
        .replace('-', "_")
}

/// 解析帧数参数。
fn parse_ticks(raw: Option<String>) -> Result<u64, RunError> {
    match raw {
        None => Ok(DEFAULT_TICKS),
        Some(text) => text
            .parse::<u64>()
            .map_err(|_| RunError::Usage(format!("帧数不是合法整数: {text}"))),
    }
}

/// 打印一次加载的结论。
fn print_report(report: &BuildReport) {
    match report {
        BuildReport::CacheHit {
            key,
            artifact,
            elapsed,
        } => {
            println!("[缓存命中]  没有调用编译器");
            println!("  缓存键    {key}");
            println!("  动态库    {}", artifact.display());
            println!("  复用耗时  {:.3} ms", elapsed.as_secs_f64() * 1000.0);
        }
        BuildReport::Compiled {
            key,
            artifact,
            elapsed,
        } => {
            println!("[缓存未命中]  已调用 rustc 编译");
            println!("  缓存键    {key}");
            println!("  动态库    {}", artifact.display());
            println!("  编译耗时  {:.3} 秒", elapsed.as_secs_f64());
        }
    }
}

/// 人类可读的字节数。
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{:.1} MB", value / (KB * KB))
    }
}

/// 人类可读的"多久之前"。
fn human_age(modified: Option<SystemTime>) -> String {
    let Some(elapsed) = modified.and_then(|time| SystemTime::now().duration_since(time).ok())
    else {
        return "未知".to_owned();
    };
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒前")
    } else if seconds < 3600 {
        format!("{} 分钟前", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} 小时前", seconds / 3600)
    } else {
        format!("{} 天前", seconds / 86_400)
    }
}

/// 命令行层面的错误。
#[derive(Debug, thiserror::Error)]
enum RunError {
    /// 参数不合法。
    #[error("用法错误: {0}")]
    Usage(String),
    /// 批量预编译部分失败。不算致命错误：其余 mod 已就绪。
    #[error("批量预编译完成，但有 {0} 个 mod 失败")]
    PartialFailure(usize),
    /// 编译或加载失败。
    #[error(transparent)]
    Loader(#[from] LoaderError),
}
