//! 编译与加载过程中的错误类型。

use std::path::PathBuf;

use thiserror::Error;

/// 编译、缓存或加载 mod 时可能出现的错误。
#[derive(Debug, Error)]
pub enum LoaderError {
    /// 文件系统操作失败。
    #[error("文件操作失败: {path}")]
    Io {
        /// 出错的路径。
        path: PathBuf,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },

    /// 无法执行 rustc。
    #[error("找不到可用的 rustc: {0}")]
    RustcUnavailable(String),

    /// rustc 以非零状态退出，附带编译器的 stderr。
    #[error("mod 编译失败:\n{0}")]
    Compile(String),

    /// 动态库加载失败。
    #[error("动态库加载失败: {path}")]
    Load {
        /// 动态库路径。
        path: PathBuf,
        /// libloading 报告的错误。
        #[source]
        source: libloading::Error,
    },

    /// mod 没有导出约定的符号。
    #[error("mod 缺少导出符号 {symbol}")]
    MissingSymbol {
        /// 缺失的符号名。
        symbol: String,
        /// libloading 报告的错误。
        #[source]
        source: libloading::Error,
    },

    /// mod 编译时使用的 ABI 与宿主不一致。
    #[error("ABI 版本不匹配: 宿主期望 {expected}, mod 报告 {found}")]
    AbiMismatch {
        /// 宿主当前支持的 ABI 版本。
        expected: u32,
        /// mod 上报的 ABI 版本。
        found: u32,
    },

    /// mod 的导出函数返回了非零状态码。
    #[error("mod 调用返回非零状态码 {status}（{kind}）")]
    ModFailed {
        /// 原始状态码。
        status: i32,
        /// 状态码的可读解释。
        kind: &'static str,
    },

    /// 宿主配置不完整或非法。
    #[error("配置错误: {message}")]
    Config {
        /// 具体缺了什么或哪里不合法。
        message: String,
    },

    /// 编译工作线程发生了 panic。
    #[error("编译 {crate_name} 时工作线程 panic，该 mod 已被跳过")]
    CompileThreadPanicked {
        /// 出问题的 crate 名。
        crate_name: String,
    },
}

/// 给 `io::Result` 补一个"附上出错路径"的便捷方法。
pub(crate) trait IoResultExt<T> {
    /// 把裸 IO 错误包装成带路径的 [`LoaderError::Io`]。
    fn at(self, path: impl Into<PathBuf>) -> Result<T, LoaderError>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T, LoaderError> {
        let path = path.into();
        self.map_err(|source| LoaderError::Io { path, source })
    }
}
