use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config in {path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("project '{0}' is not registered")]
    UnknownProject(String),
    #[error("project '{0}' is already registered")]
    DuplicateProject(String),
    #[error("'{0}' is not a valid name: use lowercase letters, digits and '-'")]
    InvalidSlug(String),
    #[error("hostname '{hostname}' is already routed by {owner}")]
    HostnameConflict { hostname: String, owner: String },
    #[error("Traefik is not set up yet; run `zevo setup`")]
    NotSetUp,
    #[error("container runtime: {0}")]
    Runtime(String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
