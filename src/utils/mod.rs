use std::env::VarError;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use indexmap::IndexMap;
use serde::de::{Error as SerdeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, de};
use std::{collections::HashMap, fmt::Display};
use tracing::warn;

use void::Void;

pub mod auto_update;
pub mod cargo;
pub mod github;
#[cfg(test)]
pub mod test;

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

pub fn get_env_or_log(env_name: String) -> Result<String, VarError> {
    std::env::var(&env_name).map_err(|e| {
        warn!("Failed to load env `{}` {}", env_name, e);
        e
    })
}
pub fn get_registry_env(registry_name: String) -> HashMap<String, String> {
    let mut envs = HashMap::from([
        (
            "CARGO_NET_GIT_FETCH_WITH_CLI".to_string(),
            "true".to_string(),
        ),
        ("GIT_SSH_COMMAND".to_string(), "ssh".to_string()),
        ("SSH_AUTH_SOCK".to_string(), "".to_string()),
    ]);
    let registry_prefix =
        format!("CARGO_REGISTRIES_{}", registry_name.replace("-", "_")).to_uppercase();
    if let Ok(index) = get_env_or_log(format!("{registry_prefix}_INDEX")) {
        envs.insert(format!("{registry_prefix}_INDEX"), index.clone());
    }
    if let Ok(token) = get_env_or_log(format!("{registry_prefix}_TOKEN")) {
        envs.insert(format!("{registry_prefix}_TOKEN"), token.clone());
        envs.insert("Authorization".to_string(), token.clone());
    }
    if let Ok(user_agent) = get_env_or_log(format!("{registry_prefix}_USER_AGENT")) {
        envs.insert("CARGO_HTTP_USER_AGENT".to_string(), user_agent.clone());
    }
    if let Ok(private_key) = get_env_or_log(format!("{registry_prefix}_PRIVATE_KEY")) {
        envs.insert(
            "GIT_SSH_COMMAND".to_string(),
            format!("ssh -i {}", private_key.clone()),
        );
    }
    envs
}
