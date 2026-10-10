//! 批量预编译的行为测试：验证"一个 mod 坏了不影响其余"。
//!
//! 这些测试会真实调用 rustc，因此要求本机有可用的 Rust 工具链——
//! 这本来就是 slilic 的运行前提。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use slilic_loader::{ModHost, ModManager, ModSource};

/// 一个能正常编译的 mod。
const GOOD_MOD: &str = r#"
use slilic_api::{export_mod, ModError, ModTickContext};

fn on_init() -> Result<(), ModError> { Ok(()) }
fn on_tick(_ctx: &ModTickContext) -> Result<(), ModError> { Ok(()) }
fn on_shutdown() -> Result<(), ModError> { Ok(()) }

export_mod!(on_init, on_tick, on_shutdown);
"#;

/// 一个必然编译失败的 mod：调用了不存在的函数。
const BAD_MOD: &str = r#"
pub fn broken() {
    totally_undefined_function_xyz();
}
"#;

/// 建一个空的临时沙箱目录。
fn temp_dir(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("slilic-it-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("创建临时目录");
    path
}

/// 定位 workspace 根，用来找 `slilic-api` 的源码。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("定位 workspace 根")
        .to_path_buf()
}

#[test]
fn precompile_separates_success_from_failure() {
    let root = workspace_root();
    let sandbox = temp_dir("precompile");

    let good_src = sandbox.join("good-mod").join("src");
    let bad_src = sandbox.join("bad-mod").join("src");
    fs::create_dir_all(&good_src).expect("建 good-mod 目录");
    fs::create_dir_all(&bad_src).expect("建 bad-mod 目录");
    fs::write(good_src.join("lib.rs"), GOOD_MOD).expect("写 good-mod 源码");
    fs::write(bad_src.join("lib.rs"), BAD_MOD).expect("写 bad-mod 源码");

    let host = ModHost::new(
        sandbox.join("cache"),
        root.join("crates")
            .join("slilic-api")
            .join("src")
            .join("lib.rs"),
        2,
    )
    .expect("建立宿主");

    let sources = [
        ModSource::new("good_mod", sandbox.join("good-mod")),
        ModSource::new("bad_mod", sandbox.join("bad-mod")),
    ];

    let first = host.precompile(&sources).expect("首次预编译");
    assert_eq!(first.compiled, 1, "好的 mod 应被实际编译");
    assert_eq!(first.cache_hits, 0);
    assert_eq!(first.failures.len(), 1, "坏的 mod 应被单独记录");
    assert!(!first.all_succeeded(), "存在失败项时不应报告全部成功");
    assert_eq!(first.succeeded(), 1);
    assert_eq!(
        first.failures[0].source.crate_name, "bad_mod",
        "失败记录应指向正确的 mod"
    );

    let second = host.precompile(&sources).expect("第二次预编译");
    assert_eq!(second.compiled, 0, "第二次不应再调用编译器");
    assert_eq!(second.cache_hits, 1, "好的 mod 应命中缓存");
    assert_eq!(second.failures.len(), 1, "坏的 mod 依然失败");

    let _ = fs::remove_dir_all(&sandbox);
}

#[test]
fn build_then_load_reuses_the_same_artifact() {
    let root = workspace_root();
    let sandbox = temp_dir("build-load");

    let src = sandbox.join("solo-mod").join("src");
    fs::create_dir_all(&src).expect("建目录");
    fs::write(src.join("lib.rs"), GOOD_MOD).expect("写源码");

    let host = ModHost::new(
        sandbox.join("cache"),
        root.join("crates")
            .join("slilic-api")
            .join("src")
            .join("lib.rs"),
        2,
    )
    .expect("建立宿主");

    let source_root = sandbox.join("solo-mod");

    let built = host.build(&source_root, "solo_mod").expect("先编译");
    let artifact = built.artifact().to_path_buf();
    assert!(
        artifact.is_file(),
        "编译后产物应当存在: {}",
        artifact.display()
    );

    let (module, report) = host.load(&source_root, "solo_mod").expect("再加载");
    assert!(report.is_cache_hit(), "编译过之后加载必须命中缓存");
    assert_eq!(report.artifact(), artifact, "应复用同一份产物");

    module.init().expect("mod 初始化");
    module.shutdown().expect("mod 卸载");

    let _ = fs::remove_dir_all(&sandbox);
}

/// 并行预编译必须把每个结果归位到正确的 mod 上，不能串位。
#[test]
fn parallel_precompile_classifies_each_mod_correctly() {
    let root = workspace_root();
    let sandbox = temp_dir("parallel");

    let mut sources = Vec::new();
    for index in 0..5_u32 {
        let dir = sandbox.join(format!("mod-{index}"));
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("建目录");
        // 第 3 个故意写坏，其余都是好的。
        let body = if index == 3 { BAD_MOD } else { GOOD_MOD };
        fs::write(src.join("lib.rs"), body).expect("写源码");
        sources.push(ModSource::new(format!("mod_{index}"), dir));
    }

    let host = ModHost::builder()
        .cache_dir(sandbox.join("cache"))
        .api_source(
            root.join("crates")
                .join("slilic-api")
                .join("src")
                .join("lib.rs"),
        )
        .opt_level(2)
        .max_parallel(4)
        .build()
        .expect("建立宿主");

    assert_eq!(host.max_parallel(), 4);

    let report = host.precompile(&sources).expect("并行预编译");
    assert_eq!(report.compiled, 4, "4 个好的 mod 应被编译");
    assert_eq!(report.cache_hits, 0);
    assert_eq!(report.failures.len(), 1, "只有第 3 个该失败");
    assert_eq!(
        report.failures[0].source.crate_name, "mod_3",
        "失败必须归到真正坏掉的那个 mod 上，不能串位"
    );

    let _ = fs::remove_dir_all(&sandbox);
}

/// 并发上限可以在运行期调整，0 表示恢复自动。
#[test]
fn max_parallel_can_be_changed_at_runtime() {
    let root = workspace_root();
    let sandbox = temp_dir("jobs");

    let host = ModHost::builder()
        .cache_dir(sandbox.join("cache"))
        .api_source(
            root.join("crates")
                .join("slilic-api")
                .join("src")
                .join("lib.rs"),
        )
        .max_parallel(2)
        .build()
        .expect("建立宿主");

    assert_eq!(host.max_parallel(), 2);
    host.set_max_parallel(7);
    assert_eq!(host.max_parallel(), 7, "运行期改并发数应立刻生效");
    host.set_max_parallel(0);
    assert!(host.max_parallel() >= 1, "0 表示按机器自动决定");

    let _ = fs::remove_dir_all(&sandbox);
}

/// 加载管理器按入队顺序推进，并把失败项单独收集。
#[test]
fn manager_advances_in_order_and_isolates_failures() {
    let root = workspace_root();
    let sandbox = temp_dir("manager");

    let good = sandbox.join("good-mod").join("src");
    let bad = sandbox.join("bad-mod").join("src");
    fs::create_dir_all(&good).expect("建 good-mod 目录");
    fs::create_dir_all(&bad).expect("建 bad-mod 目录");
    fs::write(good.join("lib.rs"), GOOD_MOD).expect("写好 mod");
    fs::write(bad.join("lib.rs"), BAD_MOD).expect("写坏 mod");

    let host = Arc::new(
        ModHost::builder()
            .cache_dir(sandbox.join("cache"))
            .api_source(
                root.join("crates")
                    .join("slilic-api")
                    .join("src")
                    .join("lib.rs"),
            )
            .build()
            .expect("建立宿主"),
    );

    let mut manager = ModManager::new(Arc::clone(&host));
    manager.enqueue(ModSource::new("good_mod", sandbox.join("good-mod")));
    manager.enqueue(ModSource::new("bad_mod", sandbox.join("bad-mod")));

    assert_eq!(manager.pending(), 2);
    assert_eq!(manager.progress(), (0, 2));

    let first = manager.advance().expect("第一个应该有结果");
    assert_eq!(first.source.crate_name, "good_mod", "必须按入队顺序推进");
    assert!(first.is_ok());

    let second = manager.advance().expect("第二个应该有结果");
    assert_eq!(second.source.crate_name, "bad_mod");
    assert!(second.is_err(), "坏 mod 应被标记为失败");

    assert!(manager.advance().is_none(), "队列空了应返回 None");

    assert_eq!(manager.processed(), 2);
    assert_eq!(manager.loaded().len(), 1);
    assert_eq!(manager.failures().len(), 1);
    assert_eq!(manager.progress(), (2, 2));

    let _ = fs::remove_dir_all(&sandbox);
}
