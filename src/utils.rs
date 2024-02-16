use std::fmt;
use std::fmt::Display;
use std::fs::read_dir;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use indexmap::IndexMap;
use serde::de::{Error as SerdeError, MapAccess, Visitor};
use serde::{de, Deserialize, Deserializer};
use void::Void;

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

pub trait FromMap {
    fn from_map(map: IndexMap<String, String>) -> Result<Self, Void>
    where
        Self: Sized;
}

fn deserialize_string_or_map<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'de> + FromStr<Err = Void> + FromMap,
    D: Deserializer<'de>,
{
    // This is a Visitor that forwards string types to T's `FromStr` impl and
    // forwards map types to T's `Deserialize` impl. The `PhantomData` is to
    // keep the compiler from complaining about T being an unused generic type
    // parameter. We need T in order to know the Value type for the Visitor
    // impl.
    struct StringOrMap<T>(PhantomData<fn() -> T>);

    impl<'de, T> Visitor<'de> for StringOrMap<T>
    where
        T: Deserialize<'de> + FromStr<Err = Void> + FromMap,
    {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or map")
        }

        fn visit_str<E>(self, value: &str) -> Result<T, E>
        where
            E: de::Error,
        {
            Ok(FromStr::from_str(value).unwrap())
        }

        fn visit_map<M>(self, map: M) -> Result<T, M::Error>
        where
            M: MapAccess<'de>,
        {
            // `MapAccessDeserializer` is a wrapper that turns a `MapAccess`
            // into a `Deserializer`, allowing it to be used as the input to T's
            // `Deserialize` implementation. T then deserializes itself using
            // the entries from the map visitor.
            match FromMap::from_map(Deserialize::deserialize(
                de::value::MapAccessDeserializer::new(map),
            )?) {
                Ok(s) => Ok(s),
                Err(_) => Err(SerdeError::custom("Should never happens")),
            }
        }
    }

    deserializer.deserialize_any(StringOrMap(PhantomData))
}

pub fn deserialize_opt_string_or_map<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de> + FromStr<Err = Void> + FromMap,
    <T as FromStr>::Err: Display,
    D: Deserializer<'de>,
{
    /// Declare an internal visitor type to handle our input.
    struct OptStringOrMap<T>(PhantomData<T>);

    impl<'de, T> de::Visitor<'de> for OptStringOrMap<T>
    where
        T: Deserialize<'de> + FromStr<Err = Void> + FromMap,
        <T as FromStr>::Err: Display,
    {
        type Value = Option<T>;

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserialize_string_or_map(deserializer).map(Some)
        }

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a null, a string or a map")
        }
    }

    d.deserialize_option(OptStringOrMap(PhantomData))
}

pub fn deserialize_opt_string_or_struct<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de> + FromStr<Err = Void>,
    D: Deserializer<'de>,
{
    /// Declare an internal visitor type to handle our input.
    struct OptStringOrStruct<T>(PhantomData<T>);

    impl<'de, T> de::Visitor<'de> for OptStringOrStruct<T>
    where
        T: Deserialize<'de> + FromStr<Err = Void>,
    {
        type Value = Option<T>;

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserialize_string_or_struct(deserializer).map(Some)
        }

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a null, a string or a map")
        }
    }

    d.deserialize_option(OptStringOrStruct(PhantomData))
}

fn deserialize_string_or_struct<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'de> + FromStr<Err = Void>,
    D: Deserializer<'de>,
{
    // This is a Visitor that forwards string types to T's `FromStr` impl and
    // forwards map types to T's `Deserialize` impl. The `PhantomData` is to
    // keep the compiler from complaining about T being an unused generic type
    // parameter. We need T in order to know the Value type for the Visitor
    // impl.
    struct StringOrStruct<T>(PhantomData<fn() -> T>);

    impl<'de, T> Visitor<'de> for StringOrStruct<T>
    where
        T: Deserialize<'de> + FromStr<Err = Void>,
    {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or map")
        }

        fn visit_str<E>(self, value: &str) -> Result<T, E>
        where
            E: de::Error,
        {
            Ok(FromStr::from_str(value).unwrap())
        }

        fn visit_map<M>(self, map: M) -> Result<T, M::Error>
        where
            M: MapAccess<'de>,
        {
            // `MapAccessDeserializer` is a wrapper that turns a `MapAccess`
            // into a `Deserializer`, allowing it to be used as the input to T's
            // `Deserialize` implementation. T then deserializes itself using
            // the entries from the map visitor.
            Deserialize::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_any(StringOrStruct(PhantomData))
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
        let expected_results = vec![path];
        assert_eq!(roots, expected_results);
    }

    #[test]
    fn test_get_cargo_roots_simple_workspace() {
        // Create fake directory structure
        let dir = TempDir::new().expect("Could not create temp dir");
        let path = dir.path();
        fs::File::create(dir.path().join("Cargo.toml")).expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_a")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_a/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        create_dir_all(dir.path().join("crates/subcrate_b")).expect("Could not create subdir");
        fs::File::create(dir.path().join("crates/subcrate_b/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        let roots = get_cargo_roots(path.to_path_buf()).expect("Could not get roots");
        let expected_results = vec![path];
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
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_a"))
            .expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_b/crates/subcrate_b"))
            .expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_c")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_a")).expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_a"))
            .expect("Could not create subdir");
        create_dir_all(dir.path().join("subdir_d/subdir_b/crates/subcrate_b"))
            .expect("Could not create subdir");
        fs::File::create(dir.path().join("subdir_a/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_a/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_b/crates/subcrate_b/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_a/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        fs::File::create(dir.path().join("subdir_d/subdir_b/Cargo.toml"))
            .expect("Could not create root Cargo.toml");
        fs::File::create(
            dir.path()
                .join("subdir_d/subdir_b/crates/subcrate_a/Cargo.toml"),
        )
        .expect("Could not create root Cargo.toml");
        fs::File::create(
            dir.path()
                .join("subdir_d/subdir_b/crates/subcrate_b/Cargo.toml"),
        )
        .expect("Could not create root Cargo.toml");

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
