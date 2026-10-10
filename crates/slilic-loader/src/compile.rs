//! rustc 调用封装。
//!
//! 这里**只调 rustc，不调 cargo**。原因是 cargo 每次都要做一遍依赖图解析和
//! 指纹计算，而 mod 的编译走的是"单个 crate + 一个显式 `--extern`"这条
//! 最短路径，直接 rustc 能把冷编译压到最短。
//!
//! 代价是 mod 暂时不能依赖 crates.io 上的第三方库。要放开这个限制，把
//! [`Toolchain::compile_mod`] 换成"生成临时 manifest + `cargo build`"即可，
//! 其余流程（缓存、加载、ABI 校验）完全不用动。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::LoaderError;

/// 本地检测到的 Rust 工具链。
#[derive(Debug, Clone)]
pub struct Toolchain {
    rustc: PathBuf,
    fingerprint: String,
    release: String,
    host_triple: String,
}

impl Toolchain {
    /// 通过 `rustc -vV` 探测本机工具链。
    ///
    /// 可用环境变量 `RUSTC` 指定 rustc 路径，便于随游戏分发包内置工具链。
    ///
    /// # Errors
    ///
    /// rustc 不存在、无法执行，或其输出缺少预期字段时返回
    /// [`LoaderError::RustcUnavailable`]。
    pub fn detect() -> Result<Self, LoaderError> {
        let rustc = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);

        let output = Command::new(&rustc)
            .arg("-vV")
            .output()
            .map_err(|err| LoaderError::RustcUnavailable(format!("{}: {err}", rustc.display())))?;

        if !output.status.success() {
            return Err(LoaderError::RustcUnavailable(format!(
                "{} -vV 退出码 {}",
                rustc.display(),
                output.status
            )));
        }

        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let release = probe_field(&text, "release").ok_or_else(|| {
            LoaderError::RustcUnavailable("rustc -vV 输出缺少 release 字段".to_owned())
        })?;
        let host_triple = probe_field(&text, "host").ok_or_else(|| {
            LoaderError::RustcUnavailable("rustc -vV 输出缺少 host 字段".to_owned())
        })?;

        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let fingerprint = format!("{:016x}", hasher.finish());

        Ok(Self {
            rustc,
            fingerprint,
            release,
            host_triple,
        })
    }

    /// 工具链指纹，参与缓存键计算。
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// rustc 的 release 版本号，如 `1.98.1`。
    #[must_use]
    pub fn release(&self) -> &str {
        &self.release
    }

    /// 宿主平台三元组，如 `x86_64-pc-windows-msvc`。
    #[must_use]
    pub fn host_triple(&self) -> &str {
        &self.host_triple
    }

    /// 把 `slilic-api` 编译成 rlib，供各 mod 作为 `--extern` 链接。
    ///
    /// # Errors
    ///
    /// rustc 执行失败或编译报错时返回 [`LoaderError::Compile`]。
    pub fn build_api_rlib(&self, api_src: &Path, output: &Path) -> Result<(), LoaderError> {
        let mut command = Command::new(&self.rustc);
        command
            .args(["--edition", "2024"])
            .args(["--crate-type", "rlib"])
            .args(["--crate-name", "slilic_api"])
            .args(["-C", "opt-level=2"])
            .args(["-C", "debuginfo=0"])
            .arg("-o")
            .arg(output)
            .arg(api_src);

        run_rustc(&mut command)
    }

    /// 把 mod 源码编译成 cdylib。
    ///
    /// 注意这里**故意不加** `-C panic=abort`：ABI 的 panic 防护依赖
    /// `catch_unwind`，而它在 abort 模式下不生效。
    ///
    /// # Errors
    ///
    /// rustc 执行失败或编译报错时返回 [`LoaderError::Compile`]，错误文本
    /// 直接来自编译器 stderr，方便转发给 mod 作者。
    pub fn compile_mod(&self, request: &CompileRequest<'_>) -> Result<(), LoaderError> {
        let mut command = Command::new(&self.rustc);
        command
            .args(["--edition", "2024"])
            .args(["--crate-type", "cdylib"])
            .args(["--crate-name", request.crate_name])
            .arg("-C")
            .arg(format!("opt-level={}", request.opt_level))
            .args(["-C", "debuginfo=0"])
            .arg("--extern")
            .arg(format!("slilic_api={}", request.api_rlib.display()))
            .arg("-o")
            .arg(request.output)
            .arg(request.source);

        run_rustc(&mut command)
    }
}

/// 一次 mod 编译请求。
#[derive(Debug)]
pub struct CompileRequest<'a> {
    /// mod 的入口源文件。
    pub source: &'a Path,
    /// `slilic-api` 的 rlib 路径。
    pub api_rlib: &'a Path,
    /// 产物输出路径。
    pub output: &'a Path,
    /// crate 名。
    pub crate_name: &'a str,
    /// 优化级别。
    pub opt_level: u8,
}

/// 从 `rustc -vV` 输出里取 `key: value` 形式的字段。
fn probe_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(": "))
            .map(|value| value.trim().to_owned())
    })
}

/// 执行 rustc，非零退出码转成带 stderr 的错误。
fn run_rustc(command: &mut Command) -> Result<(), LoaderError> {
    let output = command
        .output()
        .map_err(|err| LoaderError::RustcUnavailable(format!("无法执行编译器: {err}")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stderr.trim().is_empty() {
        Err(LoaderError::Compile(format!(
            "rustc 退出码 {}，无 stderr。stdout:\n{stdout}",
            output.status
        )))
    } else {
        Err(LoaderError::Compile(stderr))
    }
}
