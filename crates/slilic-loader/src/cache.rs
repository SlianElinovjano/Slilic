//! 内容寻址的编译缓存。
//!
//! 这是"不让他们每次都等"的全部秘密：缓存键由 mod 源码内容、ABI 版本、
//! 工具链指纹、目标平台、优化级别共同哈希得出。任何一项变了键就变，
//! 键没变就直接复用已有的动态库。
//!
//! 注意键里**没有**时间戳一类的东西——纯粹是内容寻址，所以同一个 mod
//! 在玩家机器上换台电脑、换个安装路径、甚至重新下载一次，只要源码和
//! 工具链一致，缓存就依然命中。

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{IoResultExt, LoaderError};

/// 一次编译产物对应的缓存键。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    /// 以十六进制字符串形式返回，可直接用作目录名。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 参与缓存键计算的全部输入。
#[derive(Debug)]
pub struct CacheInputs<'a> {
    /// mod 源码根目录，会递归扫描其中所有 `.rs` 文件。
    pub source_root: &'a Path,
    /// crate 名，避免不同 mod 的相同源码互相覆盖。
    pub crate_name: &'a str,
    /// 宿主支持的 ABI 版本。
    pub abi_version: u32,
    /// 工具链指纹（rustc 版本 + commit + host triple + LLVM 版本）。
    pub toolchain_fingerprint: &'a str,
    /// 目标平台三元组。
    pub target_triple: &'a str,
    /// 优化级别。
    pub opt_level: u8,
}

/// 扫描源码目录，按内容计算出缓存键。
///
/// 文件按相对路径排序后再喂给哈希器，因此结果与文件系统返回顺序无关，
/// 是可复现的。
///
/// # Errors
///
/// 源码目录不可读、或某个 `.rs` 文件读取失败时返回 [`LoaderError::Io`]。
pub fn compute_key(inputs: &CacheInputs<'_>) -> Result<CacheKey, LoaderError> {
    let mut sources = Vec::new();
    collect_sources(inputs.source_root, &mut sources)?;
    sources.sort();

    let mut hasher = DefaultHasher::new();
    inputs.crate_name.hash(&mut hasher);
    inputs.abi_version.hash(&mut hasher);
    inputs.toolchain_fingerprint.hash(&mut hasher);
    inputs.target_triple.hash(&mut hasher);
    inputs.opt_level.hash(&mut hasher);
    sources.len().hash(&mut hasher);

    let mut total_bytes: u64 = 0;
    for path in &sources {
        let relative = path.strip_prefix(inputs.source_root).unwrap_or(path);
        relative.to_string_lossy().hash(&mut hasher);
        let bytes = fs::read(path).at(path)?;
        total_bytes += bytes.len() as u64;
        bytes.hash(&mut hasher);
    }
    total_bytes.hash(&mut hasher);

    Ok(CacheKey(format!("{:016x}", hasher.finish())))
}

/// 递归收集目录下的 `.rs` 文件。
///
/// 使用 `file_type()` 而非 `metadata()`，因此不会跟随符号链接，
/// 天然避免了目录环。
fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), LoaderError> {
    let entries = fs::read_dir(dir).at(dir)?;
    for entry in entries {
        let entry = entry.at(dir)?;
        let path = entry.path();
        let file_type = entry.file_type().at(&path)?;
        if file_type.is_dir() {
            collect_sources(&path, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// 缓存目录中的一条记录。
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// 产物目录。
    pub dir: PathBuf,
    /// 已编译好的动态库路径，缺失说明上次编译中途失败了。
    pub artifact: Option<PathBuf>,
    /// 动态库字节数。
    pub bytes: u64,
    /// 动态库的最后修改时间。
    pub modified: Option<SystemTime>,
}

/// 磁盘上的编译缓存。
#[derive(Debug, Clone)]
pub struct CompileCache {
    root: PathBuf,
}

impl CompileCache {
    /// 在指定目录建立（或复用）缓存。
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 缓存根目录。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 某个键对应的产物目录（不保证存在）。
    #[must_use]
    pub fn artifact_dir(&self, crate_name: &str, key: &CacheKey) -> PathBuf {
        self.root.join(format!("{crate_name}-{key}"))
    }

    /// 查缓存。命中则返回动态库路径。
    #[must_use]
    pub fn lookup(&self, crate_name: &str, key: &CacheKey, lib_file: &str) -> Option<PathBuf> {
        let artifact = self.artifact_dir(crate_name, key).join(lib_file);
        artifact.is_file().then_some(artifact)
    }

    /// 列出缓存中的全部记录，用于 `inspect`。
    ///
    /// # Errors
    ///
    /// 缓存目录不可读时返回 [`LoaderError::Io`]；目录不存在视为空缓存。
    pub fn entries(&self) -> Result<Vec<CacheEntry>, LoaderError> {
        if !self.root.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root).at(&self.root)? {
            let entry = entry.at(&self.root)?;
            let dir = entry.path();
            if !entry.file_type().at(&dir)?.is_dir() {
                continue;
            }
            // 下划线开头的是内部目录（如 `_toolchain`），不算 mod 产物。
            if dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('_'))
            {
                continue;
            }
            let artifact = fs::read_dir(&dir)
                .at(&dir)?
                .filter_map(Result::ok)
                .map(|item| item.path())
                .find(|path| {
                    path.is_file()
                        && path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .is_some_and(|ext| matches!(ext, "dll" | "so" | "dylib"))
                });
            let (bytes, modified) = match &artifact {
                Some(path) => match fs::metadata(path).at(path) {
                    Ok(meta) => (meta.len(), meta.modified().ok()),
                    Err(_) => (0, None),
                },
                None => (0, None),
            };
            entries.push(CacheEntry {
                dir,
                artifact,
                bytes,
                modified,
            });
        }
        entries.sort_by(|a, b| a.dir.cmp(&b.dir));
        Ok(entries)
    }

    /// 清空整个缓存目录。
    ///
    /// # Errors
    ///
    /// 删除失败时返回 [`LoaderError::Io`]。
    pub fn clear(&self) -> Result<(), LoaderError> {
        if self.root.is_dir() {
            fs::remove_dir_all(&self.root).at(&self.root)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一次性的临时源码目录。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("slilic-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("src")).expect("创建临时源码目录");
            Self(path)
        }

        fn write(&self, name: &str, content: &str) {
            fs::write(self.0.join(name), content).expect("写入源码文件");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn inputs<'a>(
        root: &'a Path,
        crate_name: &'a str,
        abi_version: u32,
        fingerprint: &'a str,
    ) -> CacheInputs<'a> {
        CacheInputs {
            source_root: root,
            crate_name,
            abi_version,
            toolchain_fingerprint: fingerprint,
            target_triple: "x86_64-pc-windows-msvc",
            opt_level: 2,
        }
    }

    #[test]
    fn same_inputs_produce_same_key() {
        let dir = TempDir::new("stable");
        dir.write("src/lib.rs", "pub fn a() {}");

        let first = compute_key(&inputs(&dir.0, "demo", 1, "fp")).expect("第一次计算");
        let second = compute_key(&inputs(&dir.0, "demo", 1, "fp")).expect("第二次计算");

        assert_eq!(first, second, "同样的源码必须得到同样的缓存键");
    }

    #[test]
    fn changed_source_content_changes_key() {
        let dir = TempDir::new("content");
        dir.write("src/lib.rs", "pub fn a() {}");
        let before = compute_key(&inputs(&dir.0, "demo", 1, "fp")).expect("修改前");

        dir.write("src/lib.rs", "pub fn a() { /* 改一行 */ }");
        let after = compute_key(&inputs(&dir.0, "demo", 1, "fp")).expect("修改后");

        assert_ne!(before, after, "改了源码就必须失效缓存");
    }

    #[test]
    fn changed_abi_version_changes_key() {
        let dir = TempDir::new("abi");
        dir.write("src/lib.rs", "pub fn a() {}");

        let v1 = compute_key(&inputs(&dir.0, "demo", 1, "fp")).expect("ABI 1");
        let v2 = compute_key(&inputs(&dir.0, "demo", 2, "fp")).expect("ABI 2");

        assert_ne!(v1, v2, "ABI 版本变了必须重新编译");
    }

    #[test]
    fn changed_toolchain_changes_key() {
        let dir = TempDir::new("toolchain");
        dir.write("src/lib.rs", "pub fn a() {}");

        let old = compute_key(&inputs(&dir.0, "demo", 1, "rustc-1.97")).expect("旧工具链");
        let new = compute_key(&inputs(&dir.0, "demo", 1, "rustc-1.98")).expect("新工具链");

        assert_ne!(old, new, "换 rustc 版本必须重新编译");
    }

    #[test]
    fn changed_crate_name_changes_key() {
        let dir = TempDir::new("crate-name");
        dir.write("src/lib.rs", "pub fn a() {}");

        let first = compute_key(&inputs(&dir.0, "alpha", 1, "fp")).expect("crate alpha");
        let second = compute_key(&inputs(&dir.0, "beta", 1, "fp")).expect("crate beta");

        assert_ne!(first, second, "不同 mod 的相同源码不能共用产物目录");
    }

    #[test]
    fn added_file_changes_key() {
        let dir = TempDir::new("added-file");
        dir.write("src/lib.rs", "pub mod helper;");
        let before = compute_key(&inputs(&dir.0, "demo", 1, "fp")).expect("新增文件前");

        dir.write("src/helper.rs", "pub fn helper() {}");
        let after = compute_key(&inputs(&dir.0, "demo", 1, "fp")).expect("新增文件后");

        assert_ne!(before, after, "多模块 mod 新增文件也必须失效缓存");
    }

    #[test]
    fn lookup_misses_for_unknown_key() {
        let dir = TempDir::new("lookup");
        let cache = CompileCache::new(&dir.0);
        let key = CacheKey("0000000000000000".to_owned());

        assert!(cache.lookup("demo", &key, "mod.dll").is_none());
    }
}
