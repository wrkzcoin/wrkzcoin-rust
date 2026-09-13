// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

// Stamps the build with its git commit, which `--version` prints.
//
// `WRKZ_GIT_COMMIT` from the environment wins: scripts/release.sh sets it, and
// so does `docker build --build-arg`. Otherwise the short hash is read from the
// checkout's `.git` — the files, not the `git` program, so nothing shells out,
// and the Docker build, whose context carries only `.git/HEAD` and the refs
// (.dockerignore), is stamped too. Outside a checkout nothing is set and
// `--version` names the release alone.
//
// crates/wrkz-wallet/build.rs includes this file, so it holds no inner
// attributes or doc comments.

use std::fs;
use std::path::{Path, PathBuf};

/// The length `git rev-parse --short` gives in this repository.
const SHORT: usize = 7;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WRKZ_GIT_COMMIT");
    if std::env::var("WRKZ_GIT_COMMIT").is_ok_and(|c| !c.is_empty()) {
        return;
    }
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    if let Some(commit) = git_commit(&manifest) {
        println!("cargo:rustc-env=WRKZ_GIT_COMMIT={commit}");
    }
}

/// The abbreviated commit HEAD points at, or `None` outside a checkout.
fn git_commit(start: &Path) -> Option<String> {
    let dot_git = start.ancestors().map(|d| d.join(".git")).find(|p| p.exists())?;
    // A linked worktree's `.git` is a file naming its own git directory, which
    // holds HEAD; the branches live in the common one it points back to.
    let git_dir = if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).ok()?;
        dot_git.parent()?.join(text.strip_prefix("gitdir:")?.trim())
    } else {
        dot_git
    };
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => git_dir.join(rel.trim()),
        Err(_) => git_dir.clone(),
    };

    let head_path = git_dir.join("HEAD");
    watch(&head_path);
    let head = fs::read_to_string(&head_path).ok()?;
    let head = head.trim();
    let hash = match head.strip_prefix("ref:") {
        Some(name) => {
            let name = name.trim();
            let loose = common.join(name);
            let packed = common.join("packed-refs");
            // Watching the branch's directory as well catches a loose ref
            // written over one that was only packed.
            watch(&loose);
            watch(loose.parent()?);
            watch(&packed);
            match fs::read_to_string(&loose) {
                Ok(hash) => hash.trim().to_string(),
                Err(_) => fs::read_to_string(&packed)
                    .ok()?
                    .lines()
                    .find_map(|l| l.split_once(' ').filter(|(_, r)| *r == name).map(|(h, _)| h.to_string()))?,
            }
        }
        None => head.to_string(),
    };
    (hash.len() >= SHORT && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash[..SHORT].to_string())
}

fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
