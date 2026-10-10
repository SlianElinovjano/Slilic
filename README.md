# slilic

**把 Rust 源码当脚本发布：mod 作者给 `.rs`，玩家本机编译成原生动态库，跑起来零解释开销。**

> 状态：原型（v0.1.0）。核心链路已跑通并有测试覆盖，API 仍可能变动。

---

## 一句话

传统的原生 mod 是**发布二进制**：作者在本地交叉编译出 `.dll` / `.so`，玩家下载后直接加载。
问题是二进制和工具链、编译器版本、目标平台死绑——换台机器、换个 rustc、换个编译器版本就可能崩。

slilic 换个方向：**只发布源码，在玩家机器上编译。**

```
mod 作者                     玩家机器
   │                            │
   │  *.rs 源码                 │
   ├─────────── 下载 ──────────▶│
   │                            ├─ rustc 编译 → mod.dll
   │                            ├─ 内容寻址缓存
   │                            └─ libloading 加载 → 每帧 tick()
```

编译只发生一次（安装 / 首次启动），之后每次启动只做一次动态库加载，游戏循环里**没有任何编译器、解释器或 VM 在场**。

---

## 它不是什么

先把边界划清楚，免得误会：

| | slilic | 它不是 |
|---|---|---|
| **VS 解释器 / VM** | 编译成机器码后运行 | ❌ 没有字节码、没有 VM、没有解释循环 |
| **VS JIT / 热重载** | 编译一次，之后一直跑 | ❌ 不是「改一行代码秒级看到效果」，追求的是**运行期**零开销而非**编辑期**反馈 |
| **VS cargo 构建工具** | 是宿主侧运行时 | ❌ 不是给 mod 作者用的 `cargo build` 包装 |
| **VS WASM 插件** | 原生机器码，直接进进程 | ❌ 没有沙箱隔离，信任模型是「mod 就是本地代码」 |

---

## 核心机制

### 1. 内容寻址的编译缓存

这是「不让他们每次都等」的全部秘密。缓存键由这些输入共同哈希得出：

```
mod 源码内容  ⊕  crate 名  ⊕  ABI 版本  ⊕  rustc 工具链指纹  ⊕  目标平台  ⊕  优化级别
```

键里**没有任何路径信息**，也没有时间戳。所以玩家把游戏挪到别的盘、改安装目录、甚至重新下载一遍，只要源码和工具链一致，缓存照样命中。

实测（本机 rustc 1.98.1）：

```
$ slilic build example-mod
[缓存未命中]  已调用 rustc 编译
  缓存键    7a2fcfbbd049eff1
  编译耗时  0.846 秒

$ slilic build example-mod
[缓存命中]  没有调用编译器
  缓存键    7a2fcfbbd049eff1
  复用耗时  0.000 ms
```

编译产物落在「暂存目录 → 成功后整体 rename 进缓存」的路径上，所以**中断的编译不会留下半成品**被下次误当作缓存命中。

### 2. 极简的 C ABI 契约

宿主和每个 mod 是各自独立的编译单元，各自静态链接了一份 `std`。因此跨边界传 `String`、`Vec`、`Box<dyn Trait>`、泛型实例化——**任何带析构函数的类型——都是未定义行为**：两边对内存布局、分配器、甚至 `Drop` 实现可以完全不同。

所以规则只有一条：

> **边界上只允许 `#[repr(C)]` 的纯数据、C 兼容指针和整数状态码。**

整个跨边界表面积就这么大：

```rust
#[repr(C)]
pub struct ModTickContext {   // 16 字节，全数值
    pub tick: u64,
    pub delta_secs: f32,
    pub entity_count: u32,
}

#[repr(i32)]
pub enum ModStatus { Ok = 0, Panicked = 1, NullContext = 2, Internal = 3 }

// 四个导出符号
mod_abi_version() -> u32
mod_init()        -> i32
mod_tick(*const ModTickContext) -> i32
mod_shutdown()    -> i32
```

想传字符串？传指针 + 长度，由宿主负责读。想报错？不跨边界，`ffi_guard` 会在返回前把它压扁成 `ModStatus::Internal`。

`mod_abi_version` 是防呆关键：宿主加载时先比对版本号，不匹配直接拒绝加载。这能挡住绝大多数「编译过了但一跑就崩」的 ABI 漂移事故。

### 3. panic 隔离

让 panic 从 `extern "C"` 函数里逃逸出去是 UB，Rust 运行时会直接 `abort` 整个进程——**一个 mod 的空指针会让玩家的游戏闪退**。

`export_mod!` 宏把每个入口都包进 `catch_unwind`：

```rust
pub fn ffi_guard<F>(f: F) -> i32 where F: FnOnce() -> Result<(), ModError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(()))  => ModStatus::Ok as i32,
        Ok(Err(_))  => ModStatus::Internal as i32,
        Err(_)      => ModStatus::Panicked as i32,
    }
}
```

于是最坏情况只是**这一个 mod 失效**，宿主继续跑：

```
$ EXAMPLE_MOD_PANIC_AT=5 slilic run example-mod 8
[example-mod] 初始化完成
thread '<unnamed>' panicked at src/lib.rs:38:9: 示例 panic：...
[example-mod] 卸载，共处理 5 帧，实体峰值 1228

mod 调用返回非零状态码 1（mod 内部 panic，已被拦截）      # 进程活着，退出码 1
```

副作用：编译 mod 时**不能**加 `-C panic=abort`，否则 `catch_unwind` 失效。源码里专门标了这条。

### 4. 批量预编译：坏一个不影响其余

`ModHost::precompile` 用固定数量的工作线程并行推进，**刻意不因单个 mod 失败而中断整批**——失败项收集进报告，其余照常编译；工作线程即使 panic 也会被就地隔离成一条失败记录。

这直接对应游戏的真实需求：玩家机器上某个 mod 编译不过时，游戏仍能正常启动，只是那个 mod 不可用。

并发上限默认取机器并行度并夹在 `1..=8`——并行跑 rustc 每个进程要吃几百 MB 内存，核多不等于应该开满。而且它可以在**运行期**改（存成原子量），游戏作者能把它直接接到玩家的设置界面上。

### 5. 直接调 rustc，不调 cargo

mod 的编译走的是「单个 crate + 一个显式 `--extern`」这条最短路径。cargo 每次都要做一遍依赖图解析和指纹计算，直接 rustc 能把冷编译压到最短。

**代价**：mod 暂时不能依赖 crates.io 上的第三方库。

**但这是可逆的**——把 `Toolchain::compile_mod` 换成「生成临时 manifest + `cargo build`」即可，其余流程（缓存、加载、ABI 校验）一行都不用动。

---

## Workspace 结构

```
slilic/
├── crates/
│   ├── slilic-api/        宿主与 mod 唯一的共享依赖：ABI 契约 + export_mod! 宏
│   ├── slilic-loader/     编译、缓存、加载、多 mod 调度
│   └── slilic-cli/        演示宿主（bin: slilic）
├── example-mod/           示例 mod：作者需要写的全部内容
└── .mod-cache/            编译产物缓存（已被 .gitignore）
```

| crate | 职责 | 依赖 |
|---|---|---|
| `slilic-api` | `#[repr(C)]` 边界类型、状态码、`ffi_guard`、`export_mod!` | 无 |
| `slilic-loader` | `Toolchain`（rustc 探测/调用）、`CompileCache`、`LoadedMod`、`ModHost`、`ModManager` | `libloading`、`thiserror` |
| `slilic-cli` | 命令行演示宿主 | 上面两个 |

### 两个入口，两种节奏

- **`ModHost`** — 「确保产物可用」。`build()` 只编译不加载，`precompile()` 批量并行编译，`load()` 编译（或命中缓存）后加载。
- **`ModManager`** — 「控制加载节奏」。编译和加载是两件性质完全不同的事：

  | | 编译 | 加载 |
  |---|---|---|
  | 耗时 | 每个 mod 一秒上下 | 毫秒级 |
  | 能否并行 | 能，且应该并行 | 不能，必须串行 |
  | 线程要求 | 随便哪个线程 | 通常要求主线程 |

  所以管理器把两者摊开：`enqueue_all()` 入队，`advance()` 每帧推进一个（可以直接喂给进度条），`advance_by(n)` 限速，`finish()` 一次做完。

---

## 快速开始

需要本机有 rustc（mod 编译的前提），并支持从 `RUSTC` 环境变量指定路径——**随游戏分发一套内置工具链**就是这么做的。

```bash
git clone https://github.com/SlianElinovjano/Slilic.git slilic && cd slilic

# 1. 首次启动 / 安装阶段：批量并行预编译
cargo run -p slilic-cli -- precompile example-mod --jobs 4

# 2. 每次启动：只加载
cargo run -p slilic-cli -- load example-mod

# 3. 跑起来
cargo run -p slilic-cli -- run example-mod 32

# 连着跑两次 build，就能看到 CacheHit
cargo run -p slilic-cli -- build example-mod
cargo run -p slilic-cli -- build example-mod

# 缓存管理
cargo run -p slilic-cli -- inspect
cargo run -p slilic-cli -- clean

# 验证 panic 隔离：mod 第 5 帧主动 panic，宿主不会挂
EXAMPLE_MOD_PANIC_AT=5 cargo run -p slilic-cli -- run example-mod 8
```

跑测试（会真实调用 rustc）：

```bash
cargo test --workspace
```

---

## 写一个 mod

作者只需要三件事：依赖 `slilic-api`、写三个**普通 Rust 函数**、用宏导出。不需要手写 `unsafe`，不碰裸指针，panic 自动被拦截。

```rust
use slilic_api::{ModError, ModTickContext, export_mod};

fn on_init() -> Result<(), ModError> {
    Ok(())
}

fn on_tick(ctx: &ModTickContext) -> Result<(), ModError> {
    // ctx.tick / ctx.delta_secs / ctx.entity_count
    Ok(())
}

fn on_shutdown() -> Result<(), ModError> {
    Ok(())
}

export_mod!(on_init, on_tick, on_shutdown);
```

`example-mod/` 里有一份更完整的版本（统计帧数、汇报实体峰值）。它的 `Cargo.toml` 只为了 IDE 支持而存在——实际编译是 `slilic-loader` 直接调 rustc，不经过 cargo。

---

## 集成方需要准备什么

1. **随游戏分发一套 rustc。** 玩家机器上大概率没有 Rust 工具链。把 `RUSTC` 指过去，`Toolchain::detect` 会自动采用。
2. **安装 / 首次启动时调 `precompile`**，配上进度界面。之后每次启动只用 `load`，成本是一次动态库加载。
3. **容忍单个 mod 编译失败。** 把失败的 mod 标记为不可用，而不是阻止游戏启动。

---

## 已知限制

- **mod 不能依赖第三方 crate**（设计取舍，见上；放开只需改一个函数）。
- **需要分发 rustc**，包体不小。
- **边界类型极简**：只有 `ModTickContext` 一种上下文；给 mod 的能力（读写世界、注册组件）尚未设计，目前 mod 只能被动观察。
- **没有沙箱**：mod 是原生代码，拥有完整进程权限。信任模型等同于「玩家自己装的游戏 mod」。
- **Windows 上 cdylib 会额外产出 `.dll.exp` / `.dll.lib` / `.pdb`**，缓存目录因此不只有 `mod.dll`。
- `LoadedMod` 一旦 drop 就卸载动态库——**任何从 mod 里拿到的函数指针都必须在此之前失效**，这是调用方的责任。

---

## 路线图

- [ ] 让 mod 能用第三方依赖（切到 `cargo build` 路径，并给缓存键加上 `Cargo.lock`）
- [ ] 更丰富的跨边界 API：宿主回调表、字符串视图、组件注册
- [ ] WASM 后备通道：内存安全的 mod 走 wasmtime，原生 mod 走这条路径
- [ ] 签名校验：允许从远端分发编译产物，让玩家跳过本地编译
- [ ] 增量编译：只重编改动过的 mod

---

## License

MIT
