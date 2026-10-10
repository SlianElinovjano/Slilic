//! 宿主与 mod 之间的稳定 C ABI 契约。
//!
//! 这个 crate 是宿主与 mod **唯一**的共享依赖，两端必须针对同一份
//! [`MOD_ABI_VERSION`] 编译。
//!
//! # 为什么边界类型这么寒酸
//!
//! 宿主与每个 mod 都是各自独立的编译单元，并且各自静态链接了一份 std。
//! 因此跨边界传 `String`、`Vec`、`Box<dyn Trait>`、泛型实例化、或者任何
//! 带析构函数的类型都是**未定义行为**——两边对同一个类型的内存布局、
//! 分配器、甚至 `Drop` 实现可以完全不同。
//!
//! 规则只有一条：**边界上只允许 `#[repr(C)]` 的纯数据、C 兼容指针和
//! 整数状态码。** 想传字符串就传指针 + 长度，由宿主负责读取。
//!
//! # 修改这个 crate 的纪律
//!
//! 任何字段增删、顺序调整、类型变更都必须递增 [`MOD_ABI_VERSION`]。
//! 宿主会在加载时校验 mod 上报的版本号，不匹配直接拒绝加载——
//! 这能挡住绝大多数"编译过了但一跑就崩"的 ABI 漂移事故。

#![deny(missing_docs)]

use std::panic::{AssertUnwindSafe, catch_unwind};

/// ABI 版本号。
///
/// 任何跨边界类型的布局变更都要递增此值。宿主加载 mod 时会比对，
/// 不一致则拒绝加载（[`ModStatus::AbiMismatch`]）。
pub const MOD_ABI_VERSION: u32 = 1;

/// mod 每个 tick 收到的只读上下文。
///
/// 全部是数值字段，保证在宿主与 mod 之间布局一致。尺寸为 16 字节，
/// 字段顺序已按对齐要求排列。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModTickContext {
    /// 自世界创建以来的逻辑帧编号。
    pub tick: u64,
    /// 距离上一帧的秒数，用于与帧率无关的运算。
    pub delta_secs: f32,
    /// 当前活跃实体数量，供 mod 做规模决策。
    pub entity_count: u32,
}

/// 导出函数返回的状态码。
///
/// 定义为 `#[repr(i32)]`，因此可以直接在 C ABI 上传输。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModStatus {
    /// 调用成功。
    Ok = 0,
    /// mod 内部 panic 了，已被 [`ffi_guard`] 拦截。
    Panicked = 1,
    /// 宿主传入了空指针（属于宿主 bug，不是 mod 的问题）。
    NullContext = 2,
    /// mod 返回了业务错误。
    Internal = 3,
}

impl ModStatus {
    /// 把状态码还原成枚举，未知码返回 `None`。
    ///
    /// # Examples
    ///
    /// ```
    /// use slilic_api::ModStatus;
    ///
    /// assert_eq!(ModStatus::from_code(0), Some(ModStatus::Ok));
    /// assert_eq!(ModStatus::from_code(999), None);
    /// ```
    #[must_use]
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Ok),
            1 => Some(Self::Panicked),
            2 => Some(Self::NullContext),
            3 => Some(Self::Internal),
            _ => None,
        }
    }
}

/// mod 内部使用的错误类型。
///
/// 带 `String` 是安全的，因为它**永远不跨边界**——[`ffi_guard`] 会在
/// 返回前把它压扁成 [`ModStatus::Internal`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModError {
    /// mod 处于非法状态，无法完成本次调用。
    InvalidState(String),
}

impl std::fmt::Display for ModError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidState(msg) => write!(f, "非法状态: {msg}"),
        }
    }
}

impl std::error::Error for ModError {}

/// 宿主用于解析导出符号的符号名（含结尾 NUL，供 `libloading` 使用）。
///
/// mod 作者不应该手写这些函数，用 [`export_mod!`] 宏生成。
pub mod symbol {
    /// 返回 ABI 版本号的符号。
    pub const ABI_VERSION: &[u8] = b"mod_abi_version\0";
    /// 初始化入口。
    pub const INIT: &[u8] = b"mod_init\0";
    /// 每帧调用。
    pub const TICK: &[u8] = b"mod_tick\0";
    /// 卸载前清理。
    pub const SHUTDOWN: &[u8] = b"mod_shutdown\0";
}

/// 在 FFI 边界内执行 mod 逻辑，把 panic 转成状态码。
///
/// # 为什么必须有这一层
///
/// 让 panic 从 `extern "C"` 函数里逃逸出去是未定义行为，Rust 运行时会
/// 直接 `abort` 整个进程——玩家的游戏会因为一个 mod 的空指针直接闪退。
/// 把 mod 逻辑包在这里面，最坏情况只是这一个 mod 失效。
///
/// 注意：`catch_unwind` 在编译期 `panic = "abort"` 时不生效，所以
/// 编译 mod 时**不能**加 `-C panic=abort`。
///
/// # Examples
///
/// ```
/// use slilic_api::{ffi_guard, ModError, ModStatus};
///
/// assert_eq!(ffi_guard(|| Ok(())), ModStatus::Ok as i32);
/// assert_eq!(ffi_guard(|| Err(ModError::InvalidState("wip".into()))), ModStatus::Internal as i32);
/// // panic 会被拦截，而不是炸掉进程
/// assert_eq!(ffi_guard(|| panic!("boom")), ModStatus::Panicked as i32);
/// ```
pub fn ffi_guard<F>(f: F) -> i32
where
    F: FnOnce() -> Result<(), ModError>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => ModStatus::Ok as i32,
        Ok(Err(_err)) => ModStatus::Internal as i32,
        Err(_payload) => ModStatus::Panicked as i32,
    }
}

/// 为 mod 生成全部四个 C ABI 导出符号。
///
/// 传入三个函数：
///
/// - `init`: `fn() -> Result<(), ModError>`
/// - `tick`: `fn(&ModTickContext) -> Result<(), ModError>`
/// - `shutdown`: `fn() -> Result<(), ModError>`
///
/// # Examples
///
/// ```
/// use slilic_api::{export_mod, ModError, ModTickContext};
///
/// fn on_init() -> Result<(), ModError> { Ok(()) }
/// fn on_tick(_ctx: &ModTickContext) -> Result<(), ModError> { Ok(()) }
/// fn on_shutdown() -> Result<(), ModError> { Ok(()) }
///
/// export_mod!(on_init, on_tick, on_shutdown);
///
/// fn main() {
///     let ctx = ModTickContext::default();
///     assert_eq!(mod_abi_version(), slilic_api::MOD_ABI_VERSION);
///     assert_eq!(mod_init(), 0);
///     assert_eq!(mod_tick(&ctx), 0);
///     assert_eq!(mod_shutdown(), 0);
/// }
/// ```
#[macro_export]
macro_rules! export_mod {
    ($init:path, $tick:path, $shutdown:path) => {
        /// 返回本 mod 编译时使用的 ABI 版本。
        #[unsafe(no_mangle)]
        pub extern "C" fn mod_abi_version() -> u32 {
            $crate::MOD_ABI_VERSION
        }

        /// mod 初始化入口。
        #[unsafe(no_mangle)]
        pub extern "C" fn mod_init() -> i32 {
            $crate::ffi_guard($init)
        }

        /// mod 每帧入口。
        #[unsafe(no_mangle)]
        pub extern "C" fn mod_tick(ctx: *const $crate::ModTickContext) -> i32 {
            if ctx.is_null() {
                return $crate::ModStatus::NullContext as i32;
            }
            // SAFETY: 上面已排除空指针。宿主保证该指针在整个调用期间指向
            // 一个有效的 ModTickContext，且 on_tick 不会把它保存下来。
            let ctx = unsafe { &*ctx };
            $crate::ffi_guard(|| $tick(ctx))
        }

        /// mod 卸载入口。
        #[unsafe(no_mangle)]
        pub extern "C" fn mod_shutdown() -> i32 {
            $crate::ffi_guard($shutdown)
        }
    };
}
