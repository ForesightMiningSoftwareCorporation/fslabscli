use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use indexmap::IndexMap;
use serde::de::{Error as SerdeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, de};
use std::{collections::HashMap, fmt::Display, path::PathBuf, process::Stdio};
use tokio::io::AsyncBufReadExt;

use void::Void;

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

/// [`execute_command`] with intermediate logging disabled.
pub async fn execute_command_without_logging(
    command: &str,
    dir: &PathBuf,
    envs: &HashMap<String, String>,
) -> (String, String, bool) {
    execute_command(command, dir, envs, None, None).await
}

/// Execute the `command`, returning stdout and stderr as strings, and success state as a boolean.
///
/// Optionally, stdout and stderr can be logged asynchronously to the current process's stdout
/// during command execution. This is useful in cases where the command might hang. If the command
/// does hang, the partially complete output would never be visible without enabling this logging.
pub async fn execute_command(
    command: &str,
    dir: &PathBuf,
    envs: &HashMap<String, String>,
    log_stdout: Option<tracing::Level>,
    log_stderr: Option<tracing::Level>,
) -> (String, String, bool) {
    let shell = if cfg!(target_os = "windows") {
        "powershell.exe"
    } else {
        "bash"
    };

    let mut child = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(dunce::canonicalize(dir).expect("Failed to canonicalize"))
        .envs(envs)
        .env_remove("SSH_AUTH_SOCK") // We should pbly set this from option
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Unable to spawn command");

    let stdout = child.stdout.take().expect("Failed to get stdout");
    let mut stdout_stream = tokio::io::BufReader::new(stdout).lines();
    let mut stdout_string = String::new();

    let stderr = child.stderr.take().expect("Failed to get stderr");
    let mut stderr_stream = tokio::io::BufReader::new(stderr).lines();
    let mut stderr_string = String::new();

    loop {
        tokio::select! {
            Ok(Some(line)) = stdout_stream.next_line() =>  {
                stdout_string.push_str(&format!("{}\n", line));
                if log_stdout.is_some() {
                    tracing::event!(tracing::Level::DEBUG, " │ {}", line)
                }
            },
            Ok(Some(line)) = stderr_stream.next_line() =>  {
                stderr_string.push_str(&format!("{}\n", line));
                if log_stderr.is_some() {
                    tracing::event!(tracing::Level::DEBUG, " │ {}", line)
                }
            },
            else => break,
        }
    }

    let status = child.wait().await;

    match status {
        Ok(output) => {
            let exit_code = output.code().unwrap_or(1);
            (stdout_string.to_string(), stderr_string, exit_code == 0)
        }
        Err(e) => ("".to_string(), e.to_string(), false),
    }
}
