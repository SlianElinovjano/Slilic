//! 示例 mod：演示 mod 作者需要写的全部内容。
//!
//! 作者只需要三件事：
//! 1. 依赖 `slilic-api`（拿到 ABI 契约与 [`export_mod!`] 宏）；
//! 2. 写三个普通 Rust 函数；
//! 3. 用 `export_mod!` 把它们导出成 C ABI 符号。
//!
//! 不需要手写 `unsafe`，不需要碰裸指针，panic 会被宏自动拦截成状态码。
//!
//! [`export_mod!`]: slilic_api::export_mod

use std::sync::atomic::{AtomicU64, Ordering};

use slilic_api::{ModError, ModTickContext, export_mod};

/// 已处理的帧数。
static TICKS_SEEN: AtomicU64 = AtomicU64::new(0);
/// 观察到的实体数量峰值。
static PEAK_ENTITIES: AtomicU64 = AtomicU64::new(0);

/// 初始化：重置状态。
fn on_init() -> Result<(), ModError> {
    TICKS_SEEN.store(0, Ordering::Relaxed);
    PEAK_ENTITIES.store(0, Ordering::Relaxed);
    println!("[example-mod] 初始化完成");
    Ok(())
}

/// 每帧逻辑：统计并周期性汇报。
fn on_tick(context: &ModTickContext) -> Result<(), ModError> {
    // 设成当前 tick 号可以让 mod 主动 panic，用来验证宿主不会被拖垮。
    // 例如: set EXAMPLE_MOD_PANIC_AT=5 && slilic run example-mod 8
    if std::env::var("EXAMPLE_MOD_PANIC_AT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        == Some(context.tick)
    {
        panic!("示例 panic：用于验证 mod 崩溃不会拖垮宿主进程");
    }

    let seen = TICKS_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    PEAK_ENTITIES.fetch_max(u64::from(context.entity_count), Ordering::Relaxed);

    if seen % 8 == 0 {
        println!(
            "[example-mod] 第 {} 帧  dt={:.4}s  实体={}  峰值={}",
            context.tick,
            f64::from(context.delta_secs),
            context.entity_count,
            PEAK_ENTITIES.load(Ordering::Relaxed)
        );
    }
    Ok(())
}

/// 卸载前汇报战绩。
fn on_shutdown() -> Result<(), ModError> {
    println!(
        "[example-mod] 卸载，共处理 {} 帧，实体峰值 {}",
        TICKS_SEEN.load(Ordering::Relaxed),
        PEAK_ENTITIES.load(Ordering::Relaxed)
    );
    Ok(())
}

export_mod!(on_init, on_tick, on_shutdown);
