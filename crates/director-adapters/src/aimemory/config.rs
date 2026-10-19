//! Explicit configuration for the ai-memory substrate.
//!
//! The adapter was always injectable — anything satisfying the domain's
//! [`MemoryProvider`](director_domain::providers::MemoryProvider) trait can be
//! handed to Orqyn's loop — but building the real one meant remembering a binary
//! path, a data directory, a workspace, a project, and five timeout bounds. This
//! is that knowledge in one place, so a caller states its dependency instead of
//! assembling it inline.
//!
//! Nothing here reads the environment implicitly: [`AiMemoryConfig::from_env`]
//! is an explicit call, so a test that constructs a config directly is never at
//! the mercy of an ambient variable.
//!
//! # Why the defaults sit below the application's publish bound
//!
//! Orqyn's checkpoint round wraps publication in `MEMORY_PUBLISH_TIMEOUT` (10s,
//! in `director-app`). If the substrate's own per-call bound were longer than
//! that, the outer bound would cancel a request still in flight — and a
//! cancelled request on a stdio MCP connection leaves that connection ambiguous,
//! because the reader may be part-way through a line. Letting the transport's own
//! bound fire first means the timeout is reported in terms the transport can
//! explain, and the connection is poisoned deliberately rather than abandoned
//! mid-stream.
//!
//! These defaults are therefore all under 10s. A caller who knows its substrate
//! is slower can raise [`McpTimeouts::tool_call`] and accept that they are also
//! responsible for the interaction, which [`AiMemoryConfig::from_env`] documents
//! rather than hides.

use std::path::{Path, PathBuf};

use crate::stdio_mcp::McpTimeouts;

use super::adapter::{AiMemoryAdapter, AiMemoryAdapterError};

/// Environment variable naming the `ai-memory` executable.
pub const BINARY_ENV: &str = "AI_MEMORY_BINARY";
/// Environment variable naming the directory the substrate roots its wiki and
/// index at.
pub const DATA_DIR_ENV: &str = "AI_MEMORY_DATA_DIR";
/// Environment variable naming the workspace scopes every call.
pub const WORKSPACE_ENV: &str = "AI_MEMORY_WORKSPACE";
/// Environment variable naming the project scopes every call.
pub const PROJECT_ENV: &str = "AI_MEMORY_PROJECT";

/// How to reach an ai-memory server.
///
/// Explicit by design: no field has a machine-specific default, so a
/// configuration is either complete or obviously incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiMemoryConfig {
    /// Path to the `ai-memory` executable.
    pub binary: PathBuf,
    /// Where the substrate roots its wiki and index. Also the child's cwd.
    ///
    /// Must be a directory Orqyn may write to. Keeping Orqyn's pages here rather
    /// than in the operator's real memory is the whole point of it existing.
    pub data_dir: PathBuf,
    /// Workspace scope, sent with every call.
    pub workspace: String,
    /// Project scope, sent with every call.
    pub project: String,
    /// Bounds on every blocking operation. See the module docs for why these
    /// defaults are deliberately small.
    pub timeouts: McpTimeouts,
}

impl AiMemoryConfig {
    /// A configuration with every field stated.
    pub fn new(
        binary: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        workspace: impl Into<String>,
        project: impl Into<String>,
    ) -> Self {
        AiMemoryConfig {
            binary: binary.into(),
            data_dir: data_dir.into(),
            workspace: workspace.into(),
            project: project.into(),
            timeouts: McpTimeouts::for_checkpoint_publication(),
        }
    }

    /// Replace the timeout bounds.
    pub fn with_timeouts(mut self, timeouts: McpTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Read the configuration from the environment.
    ///
    /// Requires [`BINARY_ENV`] and [`DATA_DIR_ENV`]; [`WORKSPACE_ENV`] and
    /// [`PROJECT_ENV`] fall back to `orqyn` and `orqyn-checkpoints`. Missing
    /// required variables are named rather than defaulted, because guessing a
    /// binary path would produce a connection to the wrong substrate or to
    /// nothing.
    ///
    /// This is the only place in the crate that reads the environment, and it is
    /// only reached when a caller asks for it.
    pub fn from_env() -> Result<Self, AiMemoryAdapterError> {
        fn required(name: &str) -> Result<String, AiMemoryAdapterError> {
            std::env::var(name).map_err(|_| {
                AiMemoryAdapterError::Transport(crate::stdio_mcp::TransportError::SpawnFailed {
                    path: format!("<{name} is not set>"),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("{name} must be set to reach an ai-memory server"),
                    ),
                })
            })
        }
        fn optional(name: &str, fallback: &str) -> String {
            std::env::var(name).unwrap_or_else(|_| fallback.to_string())
        }

        Ok(AiMemoryConfig {
            binary: PathBuf::from(required(BINARY_ENV)?),
            data_dir: PathBuf::from(required(DATA_DIR_ENV)?),
            workspace: optional(WORKSPACE_ENV, "orqyn"),
            project: optional(PROJECT_ENV, "orqyn-checkpoints"),
            timeouts: McpTimeouts::for_checkpoint_publication(),
        })
    }

    /// Create the data directory if it is missing, then spawn and connect.
    ///
    /// The directory is created here rather than left to the substrate because a
    /// substrate that cannot create its own root would otherwise fail with an
    /// error that does not say which path was wrong.
    pub async fn connect(&self) -> Result<AiMemoryAdapter, AiMemoryAdapterError> {
        std::fs::create_dir_all(&self.data_dir).map_err(|source| {
            AiMemoryAdapterError::Transport(crate::stdio_mcp::TransportError::SpawnFailed {
                path: self.data_dir.to_string_lossy().into_owned(),
                source,
            })
        })?;
        AiMemoryAdapter::connect_with_timeouts(
            &self.binary.to_string_lossy(),
            Path::new(&self.data_dir),
            self.workspace.clone(),
            self.project.clone(),
            self.timeouts,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_config_needs_every_field_and_none_is_invented() {
        let c = AiMemoryConfig::new("ai-memory", "data", "ws", "proj");
        assert_eq!(c.binary, PathBuf::from("ai-memory"));
        assert_eq!(c.data_dir, PathBuf::from("data"));
        assert_eq!(c.workspace, "ws");
        assert_eq!(c.project, "proj");
    }

    #[test]
    fn the_default_bounds_sit_under_the_applications_publish_bound() {
        // The application wraps publication in a 10s bound. Every substrate bound
        // must be shorter so the transport times out first, reports in its own
        // terms, and poisons the connection deliberately.
        let app_bound = Duration::from_secs(10);
        let t = AiMemoryConfig::new("ai-memory", "data", "ws", "proj").timeouts;
        for (name, value) in [
            ("handshake", t.handshake),
            ("tool_call", t.tool_call),
            ("write", t.write),
            ("lock", t.lock),
        ] {
            assert!(
                value < app_bound,
                "{name} ({value:?}) must be under the application publish bound \
                 ({app_bound:?}), or publication cancels an in-flight request"
            );
        }
        // Shutdown happens after the round, so it is not racing that bound.
        assert!(t.shutdown <= app_bound);
    }

    /// Injectability, asserted from the crate allowed to know the substrate.
    ///
    /// Orqyn's loop is generic over the domain `MemoryProvider` trait, so the
    /// real adapter satisfies it with no bridging type: a caller can hand a live
    /// server straight to `Runner::new`. The equivalent assertion cannot live in
    /// `director-app`, which the boundary test forbids from naming a substrate —
    /// so it lives here.
    #[test]
    fn the_adapter_satisfies_the_trait_the_loop_requires() {
        fn assert_provider<T: director_domain::providers::MemoryProvider + Send + Sync>() {}
        assert_provider::<AiMemoryAdapter>();
    }

    #[test]
    fn a_config_round_trips_through_a_derived_builder() {
        let fast = McpTimeouts::fast();
        let c = AiMemoryConfig::new("ai-memory", "data", "ws", "proj").with_timeouts(fast);
        assert_eq!(c.timeouts, fast);
    }
}
