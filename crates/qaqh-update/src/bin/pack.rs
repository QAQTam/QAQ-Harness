//! qaqh-pack — 更新包打包 CLI（文件级增量 / 基线发布）。
//!
//! 子命令：
//! - `delta`：基于基线（检查点完整包 manifest）生成增量 bundle；
//!   可选输出 zip 与更新 catalog.json（strategy=file-level-delta）。
//! - `baseline-publish`：把检查点完整 bundle 的 manifest 发布为基线清单
//!   （`baselines/<version>/manifest.json`），供后续 delta 作为基线参照。
//!
//! 用法：
//! ```text
//! qaqh-pack delta --baseline <manifest.json> --target <bundle-root> \
//!     --baseline-version <v> --out <dir> \
//!     [--zip <out.zip>] [--catalog <catalog.json>] [--restart-policy daemon|electron|full]
//! qaqh-pack baseline-publish --bundle <bundle-root> --version <v> --out <baselines-root>
//! ```
//!
//! `--target` / `--bundle` 指向 collect-payload 产物根目录（含 `bundle.json` +
//! `files/`）。元数据（kind/buildId/components…）从目标 `bundle.json` 读取。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use qaqh_update::{
    Artifact, ArtifactKind, ArtifactPayload, ArtifactRequires, ArtifactStrategy, BundleManifest,
    RestartPolicy, Result, build_delta_manifest, parse_bundle_manifest, parse_catalog, plan_delta,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("");
    let result = match command {
        "delta" => cmd_delta(&args[1..]),
        "baseline-publish" => cmd_baseline_publish(&args[1..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => Err(qaqh_update::UpdateError(format!(
            "unknown command '{other}' (expected delta | baseline-publish)"
        ))),
    };
    if let Err(error) = result {
        eprintln!("qaqh-pack: {error}");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!(
        "qaqh-pack — QAQ-Harness 更新包打包 CLI\n\
         delta --baseline <manifest.json> --target <bundle-root> --baseline-version <v> --out <dir> \
         [--zip <out.zip>] [--catalog <catalog.json>] [--restart-policy daemon|electron|full]\n\
         baseline-publish --bundle <bundle-root> --version <v> --out <baselines-root>"
    );
}

/// 简易 `--key value` 参数解析（无 clap 依赖）。
struct Args {
    pairs: Vec<(String, String)>,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Self> {
        let mut pairs = Vec::new();
        let mut iter = raw.iter();
        while let Some(key) = iter.next() {
            if !key.starts_with("--") {
                return Err(qaqh_update::UpdateError(format!(
                    "expected --flag, got '{key}'"
                )));
            }
            let value = iter
                .next()
                .ok_or_else(|| qaqh_update::UpdateError(format!("missing value for {key}")))?
                .clone();
            pairs.push((key[2..].to_string(), value));
        }
        Ok(Self { pairs })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn require(&self, key: &str) -> Result<String> {
        self.get(key)
            .map(str::to_string)
            .ok_or_else(|| qaqh_update::UpdateError(format!("missing required --{key}")))
    }
}

// ── delta ────────────────────────────────────────────────

fn cmd_delta(raw: &[String]) -> Result<()> {
    let args = Args::parse(raw)?;
    let baseline_path = PathBuf::from(args.require("baseline")?);
    let target_root = PathBuf::from(args.require("target")?);
    let baseline_version = args.require("baseline-version")?;
    let out_root = PathBuf::from(args.require("out")?);

    // 1) 读基线清单 + 目标 manifest。
    let baseline = read_manifest_file(&baseline_path)?;
    let target_manifest = read_manifest_file(&target_root.join("bundle.json"))?;
    let target_files = target_root.join("files");
    if !target_files.is_dir() {
        return Err(qaqh_update::UpdateError(format!(
            "target bundle has no files/ directory: {}",
            target_files.display()
        )));
    }

    // 2) 逐文件 sha256 对比 → 变化清单。
    let changed = plan_delta(&baseline, &target_files)?;
    if changed.is_empty() {
        println!("delta: no changed files (target identical to baseline)");
    }

    // 3) 组装增量 manifest + 物化 bundle 根。
    let delta = build_delta_manifest(&target_manifest, &changed);
    fs::create_dir_all(out_root.join("files"))?;
    let manifest_path = out_root.join("bundle.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&delta)?)?;
    for file in &changed {
        let source = target_files.join(&file.target);
        let destination = out_root.join("files").join(&file.target);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &destination)?;
    }

    // 4) 可选 zip（bundle.json + files/ 整体压缩，与完整包布局一致）。
    let mut zip_path: Option<PathBuf> = None;
    if let Some(zip) = args.get("zip") {
        zip_path = Some(PathBuf::from(zip));
        zip_dir(&out_root, zip_path.as_ref().unwrap())?;
    }

    // 5) 可选 catalog 更新。
    if let Some(catalog_path) = args.get("catalog") {
        update_catalog(
            Path::new(catalog_path),
            &delta,
            &baseline_version,
            args.get("restart-policy"),
            zip_path.as_deref(),
        )?;
    }

    let total: u64 = changed.iter().map(|f| f.size).sum();
    println!(
        "delta: {} changed files, {} bytes -> {}",
        changed.len(),
        total,
        out_root.display()
    );
    Ok(())
}

fn update_catalog(
    catalog_path: &Path,
    delta: &BundleManifest,
    baseline_version: &str,
    restart_policy: Option<&str>,
    zip_path: Option<&Path>,
) -> Result<()> {
    let bytes = fs::read(catalog_path)?;
    let mut catalog = parse_catalog(&bytes)?;

    // 合并 components（目标构建的组件信息推进 catalog）。
    for (name, component) in &delta.components {
        catalog.components.insert(name.clone(), component.clone());
    }

    // 构造 artifact 条目（对齐 make-update-source.ps1 语义）。
    let kind = artifact_kind(&delta.kind)?;
    let safe_build_id = sanitize(&delta.build_id);
    let id = format!("{}-{safe_build_id}", delta.kind);
    let targets: BTreeMap<String, String> = delta
        .components
        .iter()
        .map(|(name, component)| (name.clone(), component.build_id.clone()))
        .collect();
    let mut requires = ArtifactRequires::default();
    requires.control_protocol = delta
        .components
        .get(&delta.kind)
        .and_then(|c| c.control_protocol);
    let policy = match restart_policy {
        Some("daemon") => RestartPolicy::Daemon,
        Some("electron") => RestartPolicy::Electron,
        Some("full") => RestartPolicy::Full,
        Some("none") => RestartPolicy::None,
        Some(other) => {
            return Err(qaqh_update::UpdateError(format!(
                "invalid --restart-policy '{other}'"
            )));
        }
        None => match kind {
            ArtifactKind::Backend => RestartPolicy::Daemon,
            _ => RestartPolicy::Electron,
        },
    };

    // payload：优先 zip（已压缩产物），否则对 out 目录直接测 size（zip 缺失时）。
    let (size, sha256) = if let Some(zip) = zip_path {
        file_digest(zip)?
    } else {
        return Err(qaqh_update::UpdateError(
            "--catalog 需要 --zip（artifact payload 必须是已压缩产物）".into(),
        ));
    };

    catalog.upsert_artifact(Artifact {
        id,
        kind,
        strategy: ArtifactStrategy::FileLevelDelta,
        baseline: Some(baseline_version.to_string()),
        targets,
        requires,
        restart_policy: policy,
        payload: ArtifactPayload {
            path: format!(
                "bundles/{}",
                zip_path.unwrap().file_name().unwrap().to_string_lossy()
            ),
            size,
            sha256,
        },
    });

    fs::write(catalog_path, serde_json::to_vec_pretty(&catalog)?)?;
    println!("catalog: upserted artifact into {}", catalog_path.display());
    Ok(())
}

// ── baseline-publish ─────────────────────────────────────

fn cmd_baseline_publish(raw: &[String]) -> Result<()> {
    let args = Args::parse(raw)?;
    let bundle_root = PathBuf::from(args.require("bundle")?);
    let version = args.require("version")?;
    let out_root = PathBuf::from(args.require("out")?);

    let manifest = read_manifest_file(&bundle_root.join("bundle.json"))?;
    let destination = out_root
        .join("baselines")
        .join(&version)
        .join("manifest.json");
    fs::create_dir_all(destination.parent().unwrap())?;
    fs::write(&destination, serde_json::to_vec_pretty(&manifest)?)?;
    println!(
        "baseline-publish: {} ({} files) -> {}",
        version,
        manifest.files.len(),
        destination.display()
    );
    Ok(())
}

// ── helpers ──────────────────────────────────────────────

fn read_manifest_file(path: &Path) -> Result<BundleManifest> {
    let bytes = fs::read(path)
        .map_err(|error| qaqh_update::UpdateError(format!("read {}: {error}", path.display())))?;
    parse_bundle_manifest(&bytes)
}

fn artifact_kind(kind: &str) -> Result<ArtifactKind> {
    match kind {
        "frontend" => Ok(ArtifactKind::Frontend),
        "backend" => Ok(ArtifactKind::Backend),
        "runtime" => Ok(ArtifactKind::Runtime),
        "renderer" => Ok(ArtifactKind::Renderer),
        "shell" => Ok(ArtifactKind::Shell),
        "full" => Ok(ArtifactKind::Full),
        other => Err(qaqh_update::UpdateError(format!(
            "unsupported bundle kind '{other}'"
        ))),
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn file_digest(path: &Path) -> Result<(u64, String)> {
    qaqh_update::sha256_reader(fs::File::open(path)?)
}

/// 递归压缩目录到 zip（bundle.json 在 zip 根，files/ 保持相对布局）。
fn zip_dir(src: &Path, destination: &Path) -> Result<()> {
    let file = fs::File::create(destination).map_err(|error| {
        qaqh_update::UpdateError(format!("create zip '{}': {error}", destination.display()))
    })?;
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    add_dir_to_zip(&mut archive, src, "", &options, destination)?;
    archive
        .finish()
        .map_err(|error| qaqh_update::UpdateError(format!("finish zip: {error}")))?;
    Ok(())
}

fn add_dir_to_zip(
    archive: &mut zip::ZipWriter<fs::File>,
    dir: &Path,
    prefix: &str,
    options: &zip::write::SimpleFileOptions,
    exclude: &Path,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // 排除 zip 自身（输出落在源目录内时避免自包含）。
        if path == exclude {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if file_type.is_dir() {
            add_dir_to_zip(archive, &path, &rel, options, exclude)?;
        } else if file_type.is_file() {
            archive
                .start_file(rel, *options)
                .map_err(|error| qaqh_update::UpdateError(format!("zip entry: {error}")))?;
            let mut source = fs::File::open(&path)?;
            std::io::copy(&mut source, archive)
                .map_err(|error| qaqh_update::UpdateError(format!("zip copy: {error}")))?;
        }
    }
    Ok(())
}
