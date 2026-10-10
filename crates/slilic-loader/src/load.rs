//! 动态库加载与符号解析。
//!
//! 这个模块是整个 crate 里唯一存在实质性 `unsafe` 的地方，每一处都带
//! SAFETY 注释。之所以无法避免，是因为动态加载本身就是拿一个运行期才知道
//! 的地址当作函数调用——编译器无从校验，只能由本模块的约定来保证：
//!
//! 1. 加载前已校验 ABI 版本号；
//! 2. 符号签名与 `slilic_api` 中定义的导出签名严格一致；
//! 3. [`LoadedMod`] 持有 `Library`，保证函数指针在自身存活期内始终有效。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use libloading::Library;
use slilic_api::{MOD_ABI_VERSION, ModStatus, ModTickContext, symbol};

use crate::error::LoaderError;

/// 已加载并校验通过的 mod 实例。
///
/// 持有动态库的所有权；`LoadedMod` 一旦被 drop，动态库随之卸载。
pub struct LoadedMod {
    _library: Library,
    abi_version: unsafe extern "C" fn() -> u32,
    init: unsafe extern "C" fn() -> i32,
    tick: unsafe extern "C" fn(*const ModTickContext) -> i32,
    shutdown: unsafe extern "C" fn() -> i32,
    shutdown_called: AtomicBool,
    path: PathBuf,
}

impl LoadedMod {
    /// 动态库在磁盘上的路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// mod 编译时使用的 ABI 版本（已确认与宿主一致）。
    #[must_use]
    pub fn abi_version(&self) -> u32 {
        // SAFETY: 函数指针来源与签名由 load() 保证；无参数、无副作用。
        unsafe { (self.abi_version)() }
    }

    /// 调用 mod 的初始化入口。
    ///
    /// # Errors
    ///
    /// mod 返回非零状态码时返回 [`LoaderError::ModFailed`]。
    pub fn init(&self) -> Result<(), LoaderError> {
        // SAFETY: 函数指针来源与签名由 load() 保证。
        check_status(unsafe { (self.init)() })
    }

    /// 调用 mod 的每帧入口。
    ///
    /// # Errors
    ///
    /// mod 返回非零状态码时返回 [`LoaderError::ModFailed`]。mod 内部
    /// panic 会被 [`slilic_api::ffi_guard`] 拦截成状态码，不会炸掉宿主进程。
    pub fn tick(&self, context: &ModTickContext) -> Result<(), LoaderError> {
        // SAFETY: 函数指针来源与签名由 load() 保证；context 在调用期间存活，
        // 且调用约定规定 mod 不得保存该引用。
        check_status(unsafe { (self.tick)(context) })
    }

    /// 调用 mod 的清理入口。
    ///
    /// 幂等：重复调用只会真正执行一次，第二次直接返回 `Ok`。因此
    /// [`Drop`] 也不会在显式调用之后再触发一次清理。
    ///
    /// # Errors
    ///
    /// mod 返回非零状态码时返回 [`LoaderError::ModFailed`]。
    pub fn shutdown(&self) -> Result<(), LoaderError> {
        if self.shutdown_called.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // SAFETY: 函数指针来源与签名由 load() 保证。
        check_status(unsafe { (self.shutdown)() })
    }
}

impl Drop for LoadedMod {
    fn drop(&mut self) {
        // 卸载前给 mod 一次清理机会，但只在调用方没显式调过 shutdown 时。
        // 这里刻意忽略返回值：加载已经结束，清理失败也没有可恢复的动作，
        // 不应该在 Drop 里制造 panic。
        if !self.shutdown_called.load(Ordering::Acquire) {
            let _status = unsafe { (self.shutdown)() };
        }
    }
}

/// 从磁盘加载动态库并校验 ABI。
///
/// # Errors
///
/// - 动态库无法加载：[`LoaderError::Load`]
/// - 缺少约定符号：[`LoaderError::MissingSymbol`]
/// - ABI 版本不匹配：[`LoaderError::AbiMismatch`]
pub fn load(path: &Path) -> Result<LoadedMod, LoaderError> {
    // SAFETY: 这是本 crate 的信任边界。调用方保证 path 指向由本 crate 用
    // 同一工具链、同一份 slilic-api 编译产出的动态库，而非任意第三方二进制。
    // 加载本身不执行 mod 代码（初始化函数需显式调用），因此这一步是安全的；
    // 后续通过符号表拿到的函数指针才需要逐个校验签名。
    let library = unsafe { Library::new(path) }.map_err(|source| LoaderError::Load {
        path: path.to_path_buf(),
        source,
    })?;

    let abi_version = resolve::<unsafe extern "C" fn() -> u32>(&library, symbol::ABI_VERSION)?;
    let init = resolve::<unsafe extern "C" fn() -> i32>(&library, symbol::INIT)?;
    let tick =
        resolve::<unsafe extern "C" fn(*const ModTickContext) -> i32>(&library, symbol::TICK)?;
    let shutdown = resolve::<unsafe extern "C" fn() -> i32>(&library, symbol::SHUTDOWN)?;

    // SAFETY: 上面的函数指针刚解析出来；ABI 版本查询无参数、无副作用。
    let found = unsafe { (abi_version)() };
    if found != MOD_ABI_VERSION {
        return Err(LoaderError::AbiMismatch {
            expected: MOD_ABI_VERSION,
            found,
        });
    }

    Ok(LoadedMod {
        _library: library,
        abi_version,
        init,
        tick,
        shutdown,
        shutdown_called: AtomicBool::new(false),
        path: path.to_path_buf(),
    })
}

/// 解析一个导出符号。泛型参数必须是上面四个签名之一。
fn resolve<T: Copy>(library: &Library, name: &'static [u8]) -> Result<T, LoaderError> {
    // SAFETY: name 是静态的 NUL 结尾字符串（见 slilic_api::symbol）。T 是函数
    // 指针类型，`*symbol` 复制出的指针只要 library 不被卸载就一直有效——
    // 这一点由 LoadedMod 持有 Library 所有权来保证。
    let symbol: libloading::Symbol<'_, T> =
        unsafe { library.get(name) }.map_err(|source| LoaderError::MissingSymbol {
            symbol: String::from_utf8_lossy(name)
                .trim_end_matches('\0')
                .to_owned(),
            source,
        })?;
    Ok(*symbol)
}

/// 把 C ABI 状态码转成 `Result`。
fn check_status(code: i32) -> Result<(), LoaderError> {
    match ModStatus::from_code(code) {
        Some(ModStatus::Ok) => Ok(()),
        Some(status) => Err(LoaderError::ModFailed {
            status: code,
            kind: status_kind(status),
        }),
        None => Err(LoaderError::ModFailed {
            status: code,
            kind: "未知状态码",
        }),
    }
}

/// 状态码的可读描述。
fn status_kind(status: ModStatus) -> &'static str {
    match status {
        ModStatus::Ok => "成功",
        ModStatus::Panicked => "mod 内部 panic，已被拦截",
        ModStatus::NullContext => "宿主传入了空上下文指针",
        ModStatus::Internal => "mod 报告业务错误",
    }
}
