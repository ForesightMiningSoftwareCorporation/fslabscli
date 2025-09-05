use std::{
    fs::{File, OpenOptions, create_dir_all},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use git2::Repository;

pub fn commit_all_changes(repo_path: &PathBuf, message: &str) {
    stage_all(repo_path);
    commit_repo(repo_path, message);
}

pub fn modify_file(repo_path: &Path, file_path: &str, content: &str) {
    let full_path = repo_path.join(file_path);

    // Ensure the directory exists
    create_dir_all(full_path.parent().unwrap()).expect("Failed to create directories");

    // Modify the file
    std::fs::write(&full_path, content).expect("Failed to write to file");
}

pub fn stage_file(repo_path: &PathBuf, file_path: &str) {
    let repo = Repository::open(repo_path).expect("Failed to open repo");
    let mut index = repo.index().unwrap();
    index
        .add_all([file_path].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("Failed to add files to index");
    index.write().expect("Failed to write index");
}

pub fn stage_all(repo_path: &PathBuf) {
    stage_file(repo_path, "*");
}

pub fn commit_repo(repo_path: &PathBuf, commit_message: &str) {
    let repo = Repository::open(repo_path).expect("Failed to open repo");
    let mut index = repo.index().unwrap();

    let oid = index.write_tree().unwrap();
    let signature = repo.signature().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    let parent_commit = repo
        .head()
        .ok()
        .and_then(|r| r.target())
        .and_then(|oid| repo.find_commit(oid).ok());

    if let Some(parent) = parent_commit {
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            commit_message,
            &tree,
            &[&parent],
        )
        .unwrap();
    } else {
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            commit_message,
            &tree,
            &[],
        )
        .unwrap();
    };
}

pub static FAKE_REGISTRY: &str = "fake-registry";

pub fn initialize_workspace(
    base_path: &Path,
    workspace_name: &str,
    sub_crates: Vec<&str>,
    alt_registries: Vec<&str>,
) {
    // Create lib.rs and Cargo.toml
    let workspace_dir = base_path.join(workspace_name);
    create_dir_all(&workspace_dir).unwrap();
    Command::new("cargo")
        .arg("init")
        .arg("--lib")
        .arg("--name")
        .arg(workspace_name)
        .arg("--registry")
        .arg(FAKE_REGISTRY)
        .current_dir(&workspace_dir)
        .output()
        .expect("Failed to create simple crate");

    let config_toml_dir = base_path.join(".cargo");
    create_dir_all(&config_toml_dir).unwrap();
    let config_toml = config_toml_dir.join("config.toml");
    let config_toml_content = format!(
        "[registries.{FAKE_REGISTRY}]\nindex = \"ssh://git@ssh.shipyard.rs/{FAKE_REGISTRY}/crate-index.git\""
    );
    let mut file = File::create(config_toml).unwrap();
    writeln!(file, "{config_toml_content}").unwrap();

    if !alt_registries.is_empty() {
        // Set Alternate registry for crates_g
        let cargo_toml = workspace_dir.join("Cargo.toml");
        let toml_content = format!(
            "{}\nalternate_registries=[\"{}\"]",
            r#"
[package.metadata.fslabs.publish.cargo]
publish = true
"#,
            alt_registries.join("\", \"")
        );
        let mut file = OpenOptions::new().append(true).open(cargo_toml).unwrap();
        writeln!(file, "{toml_content}").unwrap();
    }

    if !sub_crates.is_empty() {
        let cargo_toml = base_path.join(workspace_name).join("Cargo.toml");
        let toml_content = "\n[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"".to_string();
        let mut file = OpenOptions::new().append(true).open(cargo_toml).unwrap();
        writeln!(file, "{toml_content}").unwrap();
        let sub_crates_dir = base_path.join(workspace_name).join("crates");
        for sub_crate in sub_crates {
            let sub_crate_dir = sub_crates_dir.join(sub_crate);
            create_dir_all(&sub_crate_dir).unwrap();
            Command::new("cargo")
                .arg("init")
                .arg("--lib")
                .arg("--name")
                .arg(format!("{workspace_name}__{sub_crate}"))
                .arg("--registry")
                .arg(FAKE_REGISTRY)
                .current_dir(&sub_crate_dir)
                .output()
                .expect("Failed to create simple crate");
        }
    }
}

pub fn create_complex_workspace() -> PathBuf {
    let tmp = assert_fs::TempDir::new()
        .unwrap()
        .into_persistent()
        .to_path_buf();

    let repo = Repository::init(&tmp).expect("Failed to init repo");

    // Configure Git user info (required for commits)
    repo.config()
        .unwrap()
        .set_str("user.name", "Test User")
        .unwrap();
    repo.config()
        .unwrap()
        .set_str("user.email", "test@example.com")
        .unwrap();
    repo.config().unwrap().set_str("gpg.sign", "false").unwrap();

    initialize_workspace(
        &tmp,
        "workspace_a",
        vec!["crates_a", "crates_b", "crates_c"],
        vec![],
    );
    initialize_workspace(&tmp, "workspace_d", vec!["crates_e", "crates_f"], vec![]);
    initialize_workspace(&tmp, "crates_g", vec![], vec!["some_other_registries"]);

    // Setup Deps
    // workspace_d/crates_e -> workspace_a/crates_a
    Command::new("cargo")
        .arg("add")
        .arg("--offline")
        .arg("--registry")
        .arg(FAKE_REGISTRY)
        .arg("--path")
        .arg("../../../workspace_a/crates/crates_a")
        .arg("workspace_a__crates_a")
        .current_dir(tmp.join("workspace_d").join("crates").join("crates_e"))
        .output()
        .expect("Failed to add workspace_a__crates_a to workspace_d__crates_e");
    // crates_g ->  workspace_d/crates_e
    Command::new("cargo")
        .arg("add")
        .arg("--offline")
        .arg("--registry")
        .arg(FAKE_REGISTRY)
        .arg("--path")
        .arg("../workspace_d/crates/crates_e")
        .arg("workspace_d__crates_e")
        .current_dir(tmp.join("crates_g"))
        .output()
        .expect("Failed to add workspace_d__crates_e");
    // crates_g ->  workspace_a/crates_b
    Command::new("cargo")
        .arg("add")
        .arg("--offline")
        .arg("--registry")
        .arg(FAKE_REGISTRY)
        .arg("--path")
        .arg("../workspace_a/crates/crates_b")
        .arg("workspace_a__crates_b")
        .current_dir(tmp.join("crates_g"))
        .output()
        .expect("Failed to add workspace_a__crates_b");
    // Create a rust-toolchain file
    modify_file(
        &tmp,
        "rust-toolchain.toml",
        "[toolchain]\nprofile = \"default\"\n channel = \"1.88\"",
    );
    // Stage and commit initial crate
    commit_all_changes(&tmp, "Initial commit");
    dunce::canonicalize(tmp).unwrap()
}
