//! 插件包：`dshr plugin pack` 把插件文件夹打成 `<id>-<版本>.dshrplugin`（就是 zip），可以用 ed25519 签名。
//!
//! 签名的对象不是 zip 的字节（压缩元数据不稳定），而是一份规范清单：每个文件一行 `<sha256>  <相对路径>`，按路径排序。
//! 签名写在包里的 `SIGNATURE`（JSON：`{ key, signature }`，key 是 `dshr-ed25519:<base64 公钥>`）。安装时按解开的文件
//! 重新算清单再验签，所以无论从哪里（压缩包、网址、索引）装，验的都是实际装进去的文件。

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signer as _, Verifier as _};
use sha2::Digest as _;

use crate::manifest::{self, MANIFEST_FILE};

/// 包里放签名的文件。
pub const SIGNATURE_FILE: &str = "SIGNATURE";
/// 公钥的写法前缀。
pub const KEY_PREFIX: &str = "dshr-ed25519:";
/// 包的扩展名。
pub const EXTENSION: &str = crate::install::PACKAGE_EXTENSION;
/// 签名私钥在系统钥匙串里的条目名；没有时读环境变量 [`SIGNING_KEY_ENV`]（base64 的 32 字节，CI 里用）。
const SIGNING_ACCOUNT: &str = "plugin-signing";
pub const SIGNING_KEY_ENV: &str = "DSHR_PLUGIN_SIGNING_KEY";
/// 不打进包里的文件夹。
const SKIPPED_DIRS: [&str; 2] = [".git", "node_modules"];
/// 包的上限（和安装的上限一致）。
const MAX_FILES: usize = 20_000;

/// 一个打好的包。
#[derive(Debug, Clone)]
pub struct Packed {
    pub path: PathBuf,
    pub id: String,
    pub version: Option<String>,
    pub sha256: String,
    /// 签了名时是公钥（`dshr-ed25519:…`）。
    pub signer: Option<String>,
    pub files: usize,
}

/// 插件文件夹里要打进包的文件（相对路径，`/` 分隔，按路径排序）：跳过隐藏文件和文件夹、`.git`、`node_modules`、
/// `SIGNATURE`，以及 `.dshrignore` 里列的 glob。
pub fn package_files(dir: &Path) -> Result<Vec<String>, String> {
    let mut ignore = globset::GlobSetBuilder::new();
    if let Ok(text) = std::fs::read_to_string(dir.join(".dshrignore")) {
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
            let pattern = line.trim_end_matches('/');
            for glob in [pattern.to_owned(), format!("{pattern}/**")] {
                ignore.add(globset::Glob::new(&glob).map_err(|error| format!(".dshrignore 里的 `{line}` 写错了：{error}"))?);
            }
        }
    }
    let ignore = ignore.build().map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    walk(dir, dir, &ignore, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk(root: &Path, dir: &Path, ignore: &globset::GlobSet, files: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("读不了 {}：{error}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if relative == SIGNATURE_FILE || ignore.is_match(&relative) {
            continue;
        }
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            walk(root, &path, ignore, files)?;
        } else if kind.is_file() {
            files.push(relative);
            if files.len() > MAX_FILES {
                return Err(format!("文件太多（上限 {MAX_FILES} 个）"));
            }
        }
    }
    Ok(())
}

/// 规范清单：每个文件一行 `<sha256>  <相对路径>`。
pub fn content_manifest(dir: &Path, files: &[String]) -> Result<String, String> {
    let mut text = String::new();
    for file in files {
        let bytes = std::fs::read(dir.join(file)).map_err(|error| format!("读不了 {file}：{error}"))?;
        let digest: String = sha2::Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        text.push_str(&format!("{digest}  {file}\n"));
    }
    Ok(text)
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 打包。`out` 是输出文件（默认在插件文件夹的上一级，`<id>-<版本>.dshrplugin`）；给了 `key` 就签名。
pub fn pack(dir: &Path, out: Option<&Path>, key: Option<&ed25519_dalek::SigningKey>) -> Result<Packed, String> {
    let folder = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let manifest = manifest::load(dir, &folder)?;
    if !dir.join(MANIFEST_FILE).is_file() {
        return Err(format!("打包要有 {MANIFEST_FILE}（写上 id 和 version）"));
    }
    let files = package_files(dir)?;
    let listing = content_manifest(dir, &files)?;
    let signature = key.map(|key| {
        let signature = key.sign(listing.as_bytes());
        serde_json::json!({
            "key": public_key_text(&key.verifying_key()),
            "signature": base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        })
        .to_string()
    });
    let name = match &manifest.version {
        Some(version) => format!("{}-{version}.{EXTENSION}", manifest.id),
        None => format!("{}.{EXTENSION}", manifest.id),
    };
    let out = out.map(Path::to_path_buf).unwrap_or_else(|| dir.parent().unwrap_or(dir).join(name));
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for file in &files {
            let bytes = std::fs::read(dir.join(file)).map_err(|error| format!("读不了 {file}：{error}"))?;
            zip.start_file(file.as_str(), options).map_err(|error| error.to_string())?;
            std::io::Write::write_all(&mut zip, &bytes).map_err(|error| error.to_string())?;
        }
        if let Some(signature) = &signature {
            zip.start_file(SIGNATURE_FILE, options).map_err(|error| error.to_string())?;
            std::io::Write::write_all(&mut zip, signature.as_bytes()).map_err(|error| error.to_string())?;
        }
        zip.finish().map_err(|error| error.to_string())?;
    }
    let bytes = buffer.into_inner();
    std::fs::write(&out, &bytes).map_err(|error| format!("写不了 {}：{error}", out.display()))?;
    Ok(Packed {
        path: out,
        id: manifest.id,
        version: manifest.version,
        sha256: sha256_hex(&bytes),
        signer: key.map(|key| public_key_text(&key.verifying_key())),
        files: files.len(),
    })
}

/// 解开的插件文件夹里有签名时验签，返回签名的公钥；没有签名时是 `None`；签名不对时报错。
pub fn verify(dir: &Path) -> Result<Option<String>, String> {
    let Ok(text) = std::fs::read_to_string(dir.join(SIGNATURE_FILE)) else { return Ok(None) };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| "SIGNATURE 格式不对".to_owned())?;
    let key_text = value["key"].as_str().ok_or("SIGNATURE 里没有 key")?;
    let key = parse_public_key(key_text)?;
    let signature = value["signature"]
        .as_str()
        .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok())
        .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
        .ok_or("SIGNATURE 里的 signature 格式不对")?;
    let listing = content_manifest(dir, &package_files(dir)?)?;
    key.verify(listing.as_bytes(), &ed25519_dalek::Signature::from_bytes(&signature))
        .map_err(|_| "签名对不上：包里的文件被改过，或者不是这个公钥签的".to_owned())?;
    Ok(Some(key_text.to_owned()))
}

pub fn public_key_text(key: &ed25519_dalek::VerifyingKey) -> String {
    format!("{KEY_PREFIX}{}", base64::engine::general_purpose::STANDARD.encode(key.to_bytes()))
}

pub fn parse_public_key(text: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    let bytes = text
        .strip_prefix(KEY_PREFIX)
        .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text.trim()).ok())
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| format!("公钥应当写成 {KEY_PREFIX}<base64>"))?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| "公钥不对".to_owned())
}

/// 生成签名密钥，私钥存进系统钥匙串（已经有了就不覆盖，除非 `replace`）。返回公钥。
pub fn keygen(replace: bool) -> Result<String, String> {
    if !replace && let Some(key) = signing_key()? {
        return Ok(public_key_text(&key.verifying_key()));
    }
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|error| format!("取不到随机数：{error}"))?;
    let key = ed25519_dalek::SigningKey::from_bytes(&secret);
    dshr_config::credentials::save_secret(SIGNING_ACCOUNT, &base64::engine::general_purpose::STANDARD.encode(secret))
        .map_err(|error| format!("存不进系统钥匙串：{error}（也可以把 base64 私钥放进环境变量 {SIGNING_KEY_ENV}）"))?;
    Ok(public_key_text(&key.verifying_key()))
}

/// 签名私钥：钥匙串 → 环境变量。都没有时是 `None`。
pub fn signing_key() -> Result<Option<ed25519_dalek::SigningKey>, String> {
    let stored = dshr_config::credentials::load_secret(SIGNING_ACCOUNT).ok().flatten().filter(|text| !text.is_empty());
    let Some(text) = stored.or_else(|| std::env::var(SIGNING_KEY_ENV).ok().filter(|text| !text.is_empty())) else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or("签名私钥格式不对（应当是 base64 的 32 字节）")?;
    Ok(Some(ed25519_dalek::SigningKey::from_bytes(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(dir: &Path) {
        std::fs::write(dir.join("plugin.toml"), "id = \"packed\"\nversion = \"1.2.0\"\n").unwrap();
        std::fs::write(dir.join("index.js"), "export const tools = [];\n").unwrap();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/a.js"), "export const a = 1;\n").unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("node_modules/x/i.js"), "").unwrap();
        std::fs::write(dir.join(".secret"), "").unwrap();
        std::fs::write(dir.join("notes.tmp"), "").unwrap();
        std::fs::write(dir.join(".dshrignore"), "*.tmp\n").unwrap();
    }

    #[test]
    fn pack_sign_install_and_verify() {
        let source = tempfile::tempdir().unwrap();
        let dir = source.path().join("packed");
        std::fs::create_dir_all(&dir).unwrap();
        plugin(&dir);
        assert_eq!(package_files(&dir).unwrap(), ["index.js", "lib/a.js", "plugin.toml"]);

        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let packed = pack(&dir, None, Some(&key)).unwrap();
        assert_eq!(packed.path, source.path().join("packed-1.2.0.dshrplugin"));
        assert_eq!(packed.files, 3);
        let bytes = std::fs::read(&packed.path).unwrap();
        let staged = crate::install::stage_bytes(&bytes, "packed-1.2.0.dshrplugin", "test", None, Some(&packed.sha256)).unwrap();
        assert_eq!(staged.signer.as_deref(), Some(public_key_text(&key.verifying_key()).as_str()));

        // 改了一个文件：签名对不上。
        let unpacked = tempfile::tempdir().unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        archive.extract(unpacked.path()).unwrap();
        assert!(verify(unpacked.path()).unwrap().is_some());
        std::fs::write(unpacked.path().join("lib/a.js"), "export const a = 2;\n").unwrap();
        assert!(verify(unpacked.path()).unwrap_err().contains("签名对不上"));

        // 没签名的包。
        let plain = pack(&dir, Some(&source.path().join("plain.zip")), None).unwrap();
        assert!(plain.signer.is_none());
        assert!(
            crate::install::stage_bytes(&std::fs::read(&plain.path).unwrap(), "plain.zip", "test", None, None).unwrap().signer.is_none()
        );
    }

    /// 设置页“从安装包安装”：`.dshrplugin` 直接装；请求里带上包信息和发布者状态（首次、同一个、换了、去掉签名）。
    #[test]
    fn stage_package_and_publisher_status() {
        use crate::app::Publisher;
        let source = tempfile::tempdir().unwrap();
        let dir = source.path().join("packed");
        std::fs::create_dir_all(&dir).unwrap();
        plugin(&dir);
        let plugins = tempfile::tempdir().unwrap();
        let plugins_dir = plugins.path().join("plugins");

        let first = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let first_key = public_key_text(&first.verifying_key());
        let packed = pack(&dir, None, Some(&first)).unwrap();
        let staged = crate::install::stage_package(&packed.path).unwrap();
        let request = staged.request(&plugins_dir, None);
        assert_eq!(request.id, "packed");
        assert_eq!(request.publisher, Publisher::Signed { key: first_key.clone() });
        let package = request.package.clone().unwrap();
        assert_eq!(package.sha256.as_deref(), Some(packed.sha256.as_str()));
        assert_eq!(package.files, 4); // 三个文件加 SIGNATURE
        assert!(package.bytes > 0);
        assert!(request.has_script && request.compatible && !request.replaces);
        staged.commit(&plugins_dir).unwrap();

        // 同一把钥匙：受信任，并且是覆盖升级。
        let request = crate::install::stage_package(&packed.path).unwrap().request(&plugins_dir, None);
        assert_eq!(request.publisher, Publisher::Trusted { key: first_key.clone() });
        assert!(request.replaces);
        assert_eq!(request.replaces_version.as_deref(), Some("1.2.0"));

        // 换了钥匙：Changed，不 force 不让装。
        let second = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let other = pack(&dir, Some(&source.path().join("other.dshrplugin")), Some(&second)).unwrap();
        let staged = crate::install::stage_package(&other.path).unwrap();
        let request = staged.request(&plugins_dir, None);
        assert_eq!(
            request.publisher,
            Publisher::Changed { previous: first_key.clone(), now: Some(public_key_text(&second.verifying_key())) }
        );
        assert!(staged.commit(&plugins_dir).unwrap_err().contains("发布者变了"));

        // 去掉签名：同样是 Changed。
        let plain = pack(&dir, Some(&source.path().join("plain.tar.dshrplugin")), None).unwrap();
        let request = crate::install::stage_package(&plain.path).unwrap().request(&plugins_dir, None);
        assert_eq!(request.publisher, Publisher::Changed { previous: first_key, now: None });

        // 不是安装包的文件、文件夹都不行。
        let text = source.path().join("readme.txt");
        std::fs::write(&text, "hello").unwrap();
        assert!(crate::install::stage_package(&text).err().unwrap().contains("不是插件安装包"));
        let broken = source.path().join("broken.dshrplugin");
        std::fs::write(&broken, "hello").unwrap();
        assert!(crate::install::stage_package(&broken).err().unwrap().contains("已损坏"));
        assert!(crate::install::stage_package(&dir).is_err());
        assert!(crate::install::is_package_name("A-1.0.DSHRPLUGIN") && crate::install::is_package_name("x.tar.gz"));
        assert!(!crate::install::is_package_name("x.txt"));
    }
}
