//! Process-wide model preparation, loaded before admitting any requests.

use std::error::Error as StdError;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use serde::Deserialize;
use vllm_chat::{Error as ChatError, LoadModelBackendsOptions, ParserSelection, validate_parser_overrides};
use vllm_engine_core_client::{EngineCoreClientConfig, TransportMode};
use vllm_managed_engine::{ManagedEngineConfig, allocate_handshake_port};
use vllm_text::{GenerationConfigMode, backend::hf::ModelSource};

/// A model recipe in `$AGENTIC_API_HOME/model.config`, encoded as TOML.
/// Shared frontend settings are declared once; `engine_args` contains only engine settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub model: String,
    pub revision: Option<String>,
    pub tokenizer: Option<String>,
    pub tokenizer_revision: Option<String>,
    pub chat_template: Option<String>,
    pub tool_call_parser: ParserSelection,
    pub reasoning_parser: ParserSelection,
    pub generation_config: GenerationConfigMode,
    pub served_model_name: Option<String>,
    pub max_model_len: Option<NonZeroU32>,
    pub python: Option<String>,
    pub engine_args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid model.config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("invalid output parser: {0}")]
    Parser(#[from] ChatError),
    #[error("could not allocate the engine handshake endpoint: {0}")]
    Handshake(#[source] Box<dyn StdError + Send + Sync>),
}

impl ModelConfig {
    /// Load the recipe, applying an explicit model override before validation.
    /// A missing file is allowed only when the model is supplied on the command line.
    ///
    /// # Errors
    /// Returns file, TOML, parser, or conflicting engine-option errors.
    pub fn load(home: &Path, model: Option<String>) -> Result<Self, ConfigError> {
        let path = home.join("model.config");
        let mut config = match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str::<Self>(&contents)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        if let Some(model) = model {
            config.model = model;
        }
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if self.model.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "direct inference requires --model or a model in model.config".to_owned(),
            ));
        }
        for (name, value) in [
            ("revision", self.revision.as_deref()),
            ("tokenizer", self.tokenizer.as_deref()),
            ("tokenizer_revision", self.tokenizer_revision.as_deref()),
            ("served_model_name", self.served_model_name.as_deref()),
            ("python", self.python.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(ConfigError::Invalid(format!("model.config {name} must not be empty")));
            }
        }
        validate_parser_overrides(&self.tool_call_parser, &self.reasoning_parser)?;
        // These options change frontend preparation or the owned single-engine transport.
        // Let vLLM validate all remaining engine flags, including device and quantization settings.
        for argument in &self.engine_args {
            let flag = argument.split('=').next().unwrap_or(argument).replace('_', "-");
            if flag.starts_with("--data-parallel")
                || flag.starts_with("-dp")
                || matches!(
                    flag.as_str(),
                    "--" | "--config"
                        | "--model"
                        | "--revision"
                        | "--tokenizer"
                        | "--tokenizer-revision"
                        | "--tokenizer-mode"
                        | "--skip-tokenizer-init"
                        | "--hf-config-path"
                        | "--hf-overrides"
                        | "--generation-config"
                        | "--override-generation-config"
                        | "--chat-template"
                        | "--chat-template-content-format"
                        | "--default-chat-template-kwargs"
                        | "--tool-call-parser"
                        | "--reasoning-parser"
                        | "--served-model-name"
                        | "--max-model-len"
                        | "--headless"
                        | "--no-headless"
                        | "--headless-client-handshake-address"
                        | "--language-model-only"
                        | "--no-language-model-only"
                        | "--api-server-count"
                        | "-asc"
                        | "--host"
                        | "--port"
                        | "--disable-log-stats"
                        | "--shutdown-timeout"
                        | "--hf-token"
                )
            {
                return Err(ConfigError::Invalid(format!(
                    "{flag} cannot be set in engine_args; use a supported model.config field"
                )));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn served_model_name(&self) -> &str {
        self.served_model_name.as_deref().unwrap_or(&self.model)
    }

    /// Resolve the frontend and engine transport from the same model recipe.
    ///
    /// # Errors
    /// Returns validation or handshake-port allocation errors.
    pub fn engine_config(&self) -> Result<ManagedEngineConfig, ConfigError> {
        self.validate()?;
        let port = allocate_handshake_port("127.0.0.1")
            .map_err(|error| ConfigError::Handshake(error.into_boxed_dyn_error()))?;
        let mut args = self.engine_args.clone();
        for (flag, value) in [
            ("--revision", self.revision.as_deref()),
            ("--tokenizer", self.tokenizer.as_deref()),
            ("--tokenizer-revision", self.tokenizer_revision.as_deref()),
            ("--chat-template", self.chat_template.as_deref()),
            ("--served-model-name", self.served_model_name.as_deref()),
            (
                "--reasoning-parser",
                self.reasoning_parser.resolve_reasoning_name(&self.model),
            ),
        ] {
            if let Some(value) = value {
                args.extend([flag.to_owned(), value.to_owned()]);
            }
        }
        args.extend(["--generation-config".to_owned(), self.generation_config.to_string()]);
        args.push("--language-model-only".to_owned());
        if let Some(length) = self.max_model_len {
            args.extend(["--max-model-len".to_owned(), length.to_string()]);
        }
        Ok(ManagedEngineConfig {
            python: self.python.clone().unwrap_or_else(|| "python3".to_owned()),
            model: self.model.clone(),
            handshake_host: "127.0.0.1".to_owned(),
            handshake_port: port,
            data_parallel_size: 1,
            client_handshake_address: None,
            python_args: args,
        })
    }

    #[must_use]
    pub fn client_config(&self, engine: &ManagedEngineConfig, ready_timeout: Duration) -> EngineCoreClientConfig {
        EngineCoreClientConfig {
            transport_mode: TransportMode::HandshakeOwner {
                handshake_address: engine.handshake_address(),
                advertised_host: engine.handshake_host.clone(),
                engine_start_index: 0,
                engine_count: 1,
                data_parallel_size: 1,
                ready_timeout,
                local_input_address: None,
                local_output_address: None,
            },
            ..EngineCoreClientConfig::new_single(engine.handshake_address()).with_model_name(self.served_model_name())
        }
    }

    #[must_use]
    pub fn frontend_options(&self) -> LoadModelBackendsOptions {
        LoadModelBackendsOptions {
            revision: self.revision.clone(),
            tokenizer: self
                .tokenizer
                .as_ref()
                .map(|id| ModelSource {
                    id: id.clone(),
                    revision: self.tokenizer_revision.clone().or_else(|| self.revision.clone()),
                })
                .or_else(|| {
                    self.tokenizer_revision.as_ref().map(|revision| ModelSource {
                        id: self.model.clone(),
                        revision: Some(revision.clone()),
                    })
                }),
            chat_template: self.chat_template.clone(),
            generation_config: self.generation_config,
            language_model_only: true,
            ..LoadModelBackendsOptions::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_keeps_engine_and_frontend_model_preparation_aligned() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("model.config"),
            r#"
            model = "checkpoint/original"
            served_model_name = "assistant"
            revision = "model-revision"
            tokenizer = "checkpoint/tokenizer"
            tokenizer_revision = "tokenizer-revision"
            chat_template = "template.jinja"
            tool_call_parser = "none"
            reasoning_parser = "none"
            generation_config = "vllm"
            max_model_len = 8192
            python = "/venv/bin/python"
            engine_args = ["--tensor-parallel-size", "2", "--dtype", "bfloat16"]
        "#,
        )
        .unwrap();
        let recipe = ModelConfig::load(home.path(), Some("checkpoint/override".to_owned())).unwrap();
        let engine = recipe.engine_config().unwrap();
        let client = recipe.client_config(&engine, Duration::from_secs(12));
        let frontend = recipe.frontend_options();
        assert_eq!(engine.model, "checkpoint/override");
        assert_eq!(client.model_name, "assistant");
        assert_eq!(engine.python, "/venv/bin/python");
        assert_eq!(frontend.revision.as_deref(), Some("model-revision"));
        assert_eq!(
            frontend.tokenizer.unwrap(),
            ModelSource {
                id: "checkpoint/tokenizer".to_owned(),
                revision: Some("tokenizer-revision".to_owned())
            }
        );
        assert_eq!(frontend.chat_template.as_deref(), Some("template.jinja"));
        assert_eq!(frontend.generation_config, GenerationConfigMode::Vllm);
        for pair in [
            ["--tensor-parallel-size", "2"],
            ["--dtype", "bfloat16"],
            ["--revision", "model-revision"],
            ["--tokenizer-revision", "tokenizer-revision"],
            ["--chat-template", "template.jinja"],
            ["--served-model-name", "assistant"],
            ["--max-model-len", "8192"],
            ["--generation-config", "vllm"],
        ] {
            assert!(engine.python_args.windows(2).any(|args| args == pair));
        }
        let command = engine.to_command();
        let args: Vec<_> = command.get_args().map(|arg| arg.to_str().unwrap()).collect();
        assert!(args.contains(&"--headless"));
        assert!(!args.contains(&"--port"));
        let TransportMode::HandshakeOwner {
            handshake_address,
            ready_timeout,
            ..
        } = client.transport_mode
        else {
            panic!("managed engine must own its handshake");
        };
        assert_eq!(handshake_address, engine.handshake_address());
        assert_eq!(ready_timeout, Duration::from_secs(12));
    }

    #[test]
    fn missing_recipe_requires_a_model_and_invalid_recipes_are_not_ignored() {
        let home = tempfile::tempdir().unwrap();
        assert!(matches!(
            ModelConfig::load(home.path(), None),
            Err(ConfigError::Invalid(_))
        ));
        assert_eq!(
            ModelConfig::load(home.path(), Some("model".to_owned())).unwrap().model,
            "model"
        );
        for contents in [
            "model = [",
            "modle = 'typo'",
            "model = ''",
            "model = 'm'\nreasoning_parser = 'not-a-parser'",
        ] {
            std::fs::write(home.path().join("model.config"), contents).unwrap();
            assert!(ModelConfig::load(home.path(), None).is_err(), "{contents}");
        }
    }

    #[test]
    fn engine_passthrough_cannot_override_shared_preparation_or_transport() {
        for argument in [
            "--config=other.yaml",
            "--tokenizer_revision=other",
            "--data-parallel-size",
            "-dp=2",
            "--headless",
            "--chat-template",
        ] {
            let recipe = ModelConfig {
                model: "model".to_owned(),
                engine_args: vec![argument.to_owned()],
                ..ModelConfig::default()
            };
            assert!(matches!(recipe.validate(), Err(ConfigError::Invalid(_))), "{argument}");
        }
    }
}
