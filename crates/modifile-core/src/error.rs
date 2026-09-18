use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("archive: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("bad glob pattern: {0}")]
    Pattern(#[from] globset::Error),

    #[error("game pack `{pack}`: {message}")]
    Pack { pack: String, message: String },

    #[error("not found: {0}")]
    NotFound(String),

    /// GitHub's unauthenticated budget is 60/hour and even 304s spend it, so
    /// this is a routine condition rather than an exceptional one.
    #[error("GitHub rate limit exhausted ({remaining} left, resets at {reset}); set a token with `modifile auth` to raise the ceiling to 5000/hour")]
    RateLimited { remaining: u32, reset: String },

    #[error("integrity check failed for {name}: expected {expected}, got {actual}")]
    Integrity {
        name: String,
        expected: String,
        actual: String,
    },

    /// Deliberately has no override flag. Swapping a plugin under a live game
    /// corrupts the game's state and ours, and there is no version of it that
    /// is safe enough to offer a bypass for.
    #[error("{game} is running — {detail}. Close it, then try again.")]
    GameRunning { game: String, detail: String },

    /// A game directory on another machine. We cannot see that machine's
    /// processes, so only the user can vouch that the game is stopped.
    #[error(
        "{target} lives on another machine ({path}), so this cannot check whether it is \
         running. Stop the server, then confirm you have done so."
    )]
    RemoteUnverifiable { target: String, path: String },

    #[error("{0}")]
    Other(String),

    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<Error>,
    },
}

impl Error {
    pub fn other(message: impl fmt::Display) -> Self {
        Error::Other(message.to_string())
    }

    pub fn pack(pack: impl Into<String>, message: impl fmt::Display) -> Self {
        Error::Pack {
            pack: pack.into(),
            message: message.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Attach human context to an error without pulling in `anyhow`.
pub trait Context<T> {
    fn ctx(self, context: impl fmt::Display) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn ctx(self, context: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Error::Context {
            context: context.to_string(),
            source: Box::new(e.into()),
        })
    }
}
