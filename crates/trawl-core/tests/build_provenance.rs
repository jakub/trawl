//! Exercise the production build script through Cargo in disposable Git trees.
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn build(root: &Path, target: &Path) -> (String, String) {
    let output = Command::new(env!("CARGO"))
        .args(["run", "--offline", "--verbose", "--manifest-path"])
        .arg(root.join("core/Cargo.toml"))
        .current_dir(root.join("core"))
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8(output.stdout).unwrap().trim().to_owned(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn exercise(linked: bool) {
    let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("core/src")).unwrap();
    std::fs::write(
        repo.join("core/Cargo.toml"),
        "[package]\nname='provenance-fixture'\nversion='0.0.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(repo.join("core/build.rs"), include_str!("../build.rs")).unwrap();
    std::fs::write(
        repo.join("core/src/main.rs"),
        "fn main() { println!(\"{} {}\", env!(\"TRAWL_GIT_SHA\"), env!(\"TRAWL_GIT_DIRTY\")); }",
    )
    .unwrap();
    // This tracked input is outside the build script's crate directory.
    std::fs::write(repo.join("other-crate.txt"), "original\n").unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    let root = if linked {
        let worktree = temp.path().join("linked");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "linked",
                worktree.to_str().unwrap(),
            ],
        );
        assert!(worktree.join(".git").is_file());
        worktree
    } else {
        assert!(repo.join(".git").is_dir());
        repo
    };
    let target = temp.path().join("target");
    let sha = git(&root, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(build(&root, &target).0, format!("{sha} false"));
    for path in [
        ".tmp/scratch",
        ".release-tooling/helper",
        "runtime/libduckdb.so",
    ] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "untracked output").unwrap();
    }
    let (metadata, log) = build(&root, &target);
    assert_eq!(metadata, format!("{sha} false"));
    assert!(
        log.contains("Fresh provenance-fixture"),
        "untracked output must not force a rebuild: {log}"
    );

    std::fs::write(root.join("other-crate.txt"), "unstaged change\n").unwrap();
    assert_eq!(build(&root, &target).0, format!("{sha} true"));
    git(&root, &["add", "other-crate.txt"]);
    assert_eq!(build(&root, &target).0, format!("{sha} true"));
    git(&root, &["commit", "-m", "tracked change"]);
    let new_sha = git(&root, &["rev-parse", "--short", "HEAD"]);
    assert_ne!(sha, new_sha);
    assert_eq!(build(&root, &target).0, format!("{new_sha} false"));
    // A staged-only difference must clear when only the index changes.
    std::fs::write(root.join("other-crate.txt"), "staged change\n").unwrap();
    git(&root, &["add", "other-crate.txt"]);
    std::fs::write(root.join("other-crate.txt"), "unstaged change\n").unwrap();
    assert_eq!(build(&root, &target).0, format!("{new_sha} true"));
    git(&root, &["reset", "HEAD", "--", "other-crate.txt"]);
    assert_eq!(build(&root, &target).0, format!("{new_sha} false"));
    std::fs::remove_file(root.join("other-crate.txt")).unwrap();
    assert_eq!(build(&root, &target).0, format!("{new_sha} true"));
    git(&root, &["restore", "other-crate.txt"]);
    assert_eq!(build(&root, &target).0, format!("{new_sha} false"));
    git(&root, &["pack-refs", "--all"]);
    assert_eq!(build(&root, &target).0, format!("{new_sha} false"));
    let (_, log) = build(&root, &target);
    assert!(
        log.contains("Fresh provenance-fixture"),
        "missing Git paths must not force reruns: {log}"
    );
}

#[test]
fn normal_repository_metadata_tracks_only_product_changes() {
    exercise(false);
}

#[test]
fn linked_worktree_metadata_tracks_only_product_changes() {
    exercise(true);
}
