use std::fs::read_dir;
use std::path::{Path, PathBuf};

pub fn get_cargo_roots(root: PathBuf) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if Path::exists(root.join("Cargo.toml").as_path()) {
        roots.push(root);
        return Ok(roots);
    }
    for entry in read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let mut sub_roots = get_cargo_roots(entry.path())?;
            roots.append(&mut sub_roots);
        }
    }
    roots.sort();
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::create_dir_all;

    use assert_fs::TempDir;

    use crate::utils::get_cargo_roots;

    #[test]
    fn test_get_cargo_roots_simple_crate() {
        // Create fake directory structure
        let dir = TempDir::new().expect("Could not create temp dir");
        let path = dir.path();
        let file_path = dir.path().join("Cargo.toml");
        fs::File::create(file_path).expect("Could not create root Cargo.toml");
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path
        ];
        assert_eq!(roots, expected_results);
    }

    #[test]
    fn test_get_cargo_roots_simple_workspace() {
        // Create fake directory structure
        let dir = TempDir::new().expect("Could not create temp dir");
        let path = dir.path();
        fs::File::create(dir.path().join("Cargo.toml")).expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_a")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_b")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path
        ];
        assert_eq!(roots, expected_results);
    }

    #[test]
    fn test_get_cargo_roots_complex_monorepo() {
        // Create fake directory structure
        // dir
        //  - subdir_a/Cargo.toml
        //  - subdir_b/Cargo_toml
        //  - subdir_b/crates/subcrate_a/Cargo.toml
        //  - subdir_b/crates/subcrate_b/Cargo.toml
        //  - subdir_c
        //  - subdir_d/subdir_a/Cargo.toml
        //  - subdir_d/subdir_b/Cargo.tom
        //  - subdir_d/subdir_b/crates/subcrate_a/Cargo.toml
        //  - subdir_d/subdir_b/crates/subcrate_b/Cargo.toml
        let dir = TempDir::new().expect("Could not create temp dir");
        create_dir_all(dir.path().join("subdir_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_b")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_c")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_b")).expect("Could not create subdir");
        fs::File::create(dir.path().join("subdir_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/crates/subcrate_a/Cargo.toml")).expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/crates/subcrate_b/Cargo.toml")).expect("Could not create root Cargo.toml");

        let path = dir.path();
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![
            path.join("subdir_a").to_path_buf(),
            path.join("subdir_b").to_path_buf(),
            path.join("subdir_d/subdir_a").to_path_buf(),
            path.join("subdir_d/subdir_b").to_path_buf(),
        ];
        assert_eq!(roots, expected_results);
    }
}
