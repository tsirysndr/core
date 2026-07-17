use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jacquard_lexicon::codegen::{CodeGenerator, CodegenMode};
use jacquard_lexicon::corpus::LexiconCorpus;
use walkdir::WalkDir;

const DEFAULT_LEXICONS_SUBDIR: &str = "lexicons";
const STAGED_SUBDIR: &str = "lexicons-staged";
const GENERATED_SUBDIR: &str = "src/_lex";
const TEMP_SEGMENT: &str = "temp";

fn main() -> Result<()> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("resolve workspace root from manifest dir")?
        .to_path_buf();

    let lexicons_dir = std::env::var("BOBBIN_LEXICONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join(DEFAULT_LEXICONS_SUBDIR));

    println!("cargo:rerun-if-env-changed=BOBBIN_LEXICONS_DIR");
    println!("cargo:rerun-if-changed={}", lexicons_dir.display());
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);

    let staged = out_dir.join(STAGED_SUBDIR);
    if staged.exists() {
        std::fs::remove_dir_all(&staged).context("clean staged lexicons")?;
    }
    stage_lexicons(&lexicons_dir, &staged)?;

    let corpus = LexiconCorpus::load_from_dir(&staged)
        .map_err(|e| anyhow::anyhow!("load lexicon corpus: {e:?}"))?;

    let generated = manifest_dir.join(GENERATED_SUBDIR);
    if generated.exists() {
        std::fs::remove_dir_all(&generated).context("clean generated dir")?;
    }
    std::fs::create_dir_all(&generated).context("create generated dir")?;

    let codegen = CodeGenerator::with_mode(&corpus, "crate", CodegenMode::Pretty);
    codegen
        .write_to_disk(&generated)
        .map_err(|e| anyhow::anyhow!("write generated code: {e:?}"))?;

    Ok(())
}

fn stage_lexicons(src: &Path, dst: &Path) -> Result<()> {
    WalkDir::new(src)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .filter(|e| !e.path().components().any(|c| c.as_os_str() == TEMP_SEGMENT))
        .try_for_each(|entry| -> Result<()> {
            let rel = entry.path().strip_prefix(src).context("strip src prefix")?;
            let target = dst.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).context("create staged parent")?;
            }
            std::fs::copy(entry.path(), &target).context("copy lexicon file")?;
            Ok(())
        })
}
