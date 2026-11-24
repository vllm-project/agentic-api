//! Codex model catalog projections and launcher defaults.
//!
//! The core capability resolver supplies the same explicit model configuration to this catalog and
//! request admission. Catalog-only defaults and descriptions remain presentation concerns here.

use std::borrow::Cow;

use agentic_core::types::ReasoningEffortLevel;
pub use agentic_core::types::model_capabilities::{
    DEFAULT_REASONING_LEVELS, InputModalities, Modality, ModalityError, ModelCapabilities, ModelOverride,
    ReasoningLevelsError, SupportedReasoningLevels, UpstreamCapabilities,
};
use serde::{Deserialize, Serialize};

/// The subset of the gateway's Codex model catalog a launcher needs.
///
/// Every other catalog field is ignored so that catalog growth cannot break a launcher.
#[derive(Debug, Default, Deserialize)]
pub struct CodexCatalogCapabilities {
    /// Catalog entries in the order the gateway advertises them.
    #[serde(default)]
    pub models: Vec<CodexModelCapabilities>,
}

/// One reasoning level as the gateway advertises it.
///
/// The description is carried rather than re-derived, so a launcher catalog and the HTTP catalog
/// describe a level the same way.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CodexReasoningLevel {
    /// The effort the model accepts.
    pub effort: ReasoningEffortLevel,
    /// The Codex-facing description of the level.
    pub description: Cow<'static, str>,
}

impl CodexReasoningLevel {
    /// Describe `effort` with the gateway's standard Codex-facing description.
    ///
    /// Used when a level comes from configuration or from the defaults. A description decoded
    /// from a gateway catalog is kept as advertised instead.
    #[must_use]
    pub fn from_effort(effort: ReasoningEffortLevel) -> Self {
        Self {
            effort,
            description: Cow::Borrowed(reasoning_level_description(effort)),
        }
    }
}

/// The Codex-facing description of a reasoning effort level.
#[must_use]
pub const fn reasoning_level_description(level: ReasoningEffortLevel) -> &'static str {
    match level {
        ReasoningEffortLevel::None => "Responses without reasoning",
        ReasoningEffortLevel::Minimal => "Minimal reasoning for quick responses",
        ReasoningEffortLevel::Low => "Fast responses with lighter reasoning",
        ReasoningEffortLevel::Medium => "Balances speed and reasoning depth",
        ReasoningEffortLevel::High => "Greater reasoning depth for complex problems",
        ReasoningEffortLevel::Xhigh => "Extended reasoning depth for demanding problems",
        ReasoningEffortLevel::Max => "Maximum reasoning depth for the hardest problems",
    }
}

/// Describe the default levels in the order they are advertised.
#[must_use]
pub fn default_codex_reasoning_levels() -> Vec<CodexReasoningLevel> {
    DEFAULT_REASONING_LEVELS
        .iter()
        .copied()
        .map(CodexReasoningLevel::from_effort)
        .collect()
}

/// One catalog entry's model identity and resolved capabilities.
#[derive(Clone, Debug, Deserialize)]
pub struct CodexModelCapabilities {
    /// Served model ID, matched exactly and case-sensitively.
    pub slug: String,
    /// The modalities the gateway resolved for this model.
    pub input_modalities: InputModalities,
    /// Whether Codex may request reasoning summaries and encrypted reasoning.
    #[serde(default)]
    pub supports_reasoning_summaries: bool,
    /// The reasoning levels the gateway advertised, in catalog order.
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
}

impl CodexModelCapabilities {
    /// The advertised reasoning levels, or the gateway defaults when the catalog omits them.
    ///
    /// A gateway older than `supported_reasoning_levels` keeps working: the launcher falls back
    /// to the default levels instead of writing a catalog without any.
    #[must_use]
    pub fn reasoning_levels(&self) -> Cow<'_, [CodexReasoningLevel]> {
        if self.supported_reasoning_levels.is_empty() {
            Cow::Owned(default_codex_reasoning_levels())
        } else {
            Cow::Borrowed(&self.supported_reasoning_levels)
        }
    }

    /// The level Codex starts on: `medium` when it is advertised, otherwise the first level.
    #[must_use]
    pub fn default_reasoning_level(&self) -> ReasoningEffortLevel {
        if self.supported_reasoning_levels.is_empty() {
            first_or_medium(DEFAULT_REASONING_LEVELS.iter().copied())
        } else {
            first_or_medium(self.supported_reasoning_levels.iter().map(|level| level.effort))
        }
    }
}

/// `medium` when it appears in `levels`, otherwise the first level, otherwise `medium`.
fn first_or_medium(levels: impl Iterator<Item = ReasoningEffortLevel>) -> ReasoningEffortLevel {
    let mut first = None;
    for level in levels {
        first.get_or_insert(level);
        if level == ReasoningEffortLevel::Medium {
            return level;
        }
    }
    first.unwrap_or(ReasoningEffortLevel::Medium)
}

impl CodexCatalogCapabilities {
    /// Select `model`, or the first advertised entry when no model was requested.
    #[must_use]
    pub fn select(&self, model: Option<&str>) -> Option<&CodexModelCapabilities> {
        match model {
            Some(model) => self.models.iter().find(|entry| entry.slug == model),
            None => self.models.first(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use agentic_core::types::messages::ReasoningEffortLevel;
    use serde::Deserialize;

    use super::{
        CodexCatalogCapabilities, DEFAULT_REASONING_LEVELS, InputModalities, Modality, ModalityError,
        ModelCapabilities, ModelOverride, ReasoningLevelsError, SupportedReasoningLevels, UpstreamCapabilities,
    };

    #[derive(Debug, Deserialize)]
    struct ModelSection {
        input_modalities: InputModalities,
    }

    #[derive(Debug, Deserialize)]
    struct ReasoningSection {
        supported_reasoning_levels: SupportedReasoningLevels,
    }

    /// An override carrying only the listed input modalities.
    fn modalities(modalities: InputModalities) -> ModelOverride {
        ModelOverride {
            input_modalities: Some(modalities),
            ..ModelOverride::default()
        }
    }

    /// An override carrying only the listed reasoning levels.
    fn levels(levels: &[ReasoningEffortLevel]) -> ModelOverride {
        ModelOverride {
            supported_reasoning_levels: Some(
                SupportedReasoningLevels::try_new(levels).expect("valid reasoning levels"),
            ),
            ..ModelOverride::default()
        }
    }

    #[test]
    fn validation_canonicalizes_accepted_modality_lists() {
        assert_eq!(InputModalities::try_new(&[Modality::Text]), Ok(InputModalities::Text));
        assert_eq!(
            InputModalities::try_new(&[Modality::Text, Modality::Image]),
            Ok(InputModalities::TextAndImage)
        );
        assert_eq!(
            InputModalities::try_new(&[Modality::Image, Modality::Text]),
            Ok(InputModalities::TextAndImage),
            "declaration order must not change the resolved modalities"
        );
    }

    #[test]
    fn validation_rejects_unusable_modality_lists() {
        assert_eq!(InputModalities::try_new(&[]), Err(ModalityError::Empty));
        assert_eq!(
            InputModalities::try_new(&[Modality::Image]),
            Err(ModalityError::MissingText)
        );
        assert_eq!(
            InputModalities::try_new(&[Modality::Text, Modality::Text]),
            Err(ModalityError::Duplicate(Modality::Text))
        );
        assert_eq!(
            InputModalities::try_new(&[Modality::Text, Modality::Image, Modality::Image]),
            Err(ModalityError::Duplicate(Modality::Image))
        );
    }

    #[test]
    fn modality_errors_name_the_wire_values() {
        assert!(ModalityError::Empty.to_string().contains("[\"text\", \"image\"]"));
        assert_eq!(
            ModalityError::Duplicate(Modality::Image).to_string(),
            "input_modalities lists \"image\" more than once"
        );
        assert!(ModalityError::MissingText.to_string().contains("must include \"text\""));
    }

    #[test]
    fn modalities_serialize_in_canonical_wire_order() {
        assert_eq!(
            serde_json::to_string(&InputModalities::Text).expect("serialize text"),
            "[\"text\"]"
        );
        assert_eq!(
            serde_json::to_string(&InputModalities::TextAndImage).expect("serialize text and image"),
            "[\"text\",\"image\"]"
        );
    }

    #[test]
    fn modalities_round_trip_through_json_and_toml() {
        let parsed: InputModalities = serde_json::from_str("[\"image\",\"text\"]").expect("parse JSON modalities");
        assert_eq!(parsed, InputModalities::TextAndImage);

        let section: ModelSection =
            toml::from_str("input_modalities = [\"text\", \"image\"]").expect("parse TOML modalities");
        assert_eq!(section.input_modalities, InputModalities::TextAndImage);
    }

    #[test]
    fn unknown_modality_names_the_accepted_values() {
        let error = toml::from_str::<ModelSection>("input_modalities = [\"video\"]")
            .expect_err("an unknown modality must be rejected");
        let message = error.to_string();

        assert!(message.contains("video"), "{message}");
        assert!(message.contains("text"), "{message}");
        assert!(message.contains("image"), "{message}");
    }

    #[test]
    fn invalid_modality_lists_are_rejected_while_parsing() {
        let error =
            toml::from_str::<ModelSection>("input_modalities = []").expect_err("an empty list must be rejected");
        assert!(error.to_string().contains("at least one modality"), "{error}");

        let error = toml::from_str::<ModelSection>("input_modalities = [\"image\"]")
            .expect_err("an image-only list must be rejected");
        assert!(error.to_string().contains("must include \"text\""), "{error}");

        let error = toml::from_str::<ModelSection>("input_modalities = [\"text\", \"text\"]")
            .expect_err("a duplicated modality must be rejected");
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn validation_keeps_accepted_reasoning_levels_in_order() {
        assert_eq!(
            SupportedReasoningLevels::try_new(&[ReasoningEffortLevel::Max, ReasoningEffortLevel::Low])
                .expect("valid levels")
                .as_slice(),
            [ReasoningEffortLevel::Max, ReasoningEffortLevel::Low],
            "declaration order must be preserved"
        );
        assert!(SupportedReasoningLevels::try_new(&[ReasoningEffortLevel::Xhigh]).is_ok());
    }

    #[test]
    fn validation_rejects_unusable_reasoning_level_lists() {
        assert_eq!(SupportedReasoningLevels::try_new(&[]), Err(ReasoningLevelsError::Empty));
        assert_eq!(
            SupportedReasoningLevels::try_new(&[ReasoningEffortLevel::High, ReasoningEffortLevel::High]),
            Err(ReasoningLevelsError::Duplicate(ReasoningEffortLevel::High))
        );
    }

    #[test]
    fn reasoning_level_errors_name_the_wire_values() {
        assert!(
            ReasoningLevelsError::Empty
                .to_string()
                .contains("[\"low\", \"medium\", \"high\"]")
        );
        assert_eq!(
            ReasoningLevelsError::Duplicate(ReasoningEffortLevel::Xhigh).to_string(),
            "supported_reasoning_levels lists \"xhigh\" more than once"
        );
    }

    #[test]
    fn reasoning_levels_round_trip_through_json_and_toml() {
        let parsed: SupportedReasoningLevels = serde_json::from_str("[\"max\",\"low\"]").expect("parse JSON levels");
        assert_eq!(
            parsed.as_slice(),
            [ReasoningEffortLevel::Max, ReasoningEffortLevel::Low]
        );
        assert_eq!(
            serde_json::to_string(&parsed).expect("serialize levels"),
            "[\"max\",\"low\"]"
        );

        let section: ReasoningSection =
            toml::from_str("supported_reasoning_levels = [\"low\", \"high\", \"max\"]").expect("parse TOML levels");
        assert_eq!(
            section.supported_reasoning_levels.as_slice(),
            [
                ReasoningEffortLevel::Low,
                ReasoningEffortLevel::High,
                ReasoningEffortLevel::Max
            ]
        );
    }

    #[test]
    fn unknown_reasoning_level_names_the_accepted_values() {
        let error = toml::from_str::<ReasoningSection>("supported_reasoning_levels = [\"ultra\"]")
            .expect_err("an unknown level must be rejected");
        let message = error.to_string();

        assert!(message.contains("ultra"), "{message}");
        assert!(message.contains("xhigh"), "{message}");
        assert!(message.contains("max"), "{message}");
    }

    #[test]
    fn invalid_reasoning_level_lists_are_rejected_while_parsing() {
        let error = toml::from_str::<ReasoningSection>("supported_reasoning_levels = []")
            .expect_err("an empty list must be rejected");
        assert!(error.to_string().contains("at least one level"), "{error}");

        let error = toml::from_str::<ReasoningSection>("supported_reasoning_levels = [\"low\", \"low\"]")
            .expect_err("a duplicated level must be rejected");
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn modalities_report_their_members() {
        assert_eq!(InputModalities::Text.as_slice(), [Modality::Text]);
        assert_eq!(
            InputModalities::TextAndImage.as_slice(),
            [Modality::Text, Modality::Image]
        );
        assert!(InputModalities::Text.contains(Modality::Text));
        assert!(!InputModalities::Text.contains(Modality::Image));
        assert!(InputModalities::TextAndImage.contains(Modality::Image));
        assert!(!InputModalities::Text.supports_image());
        assert!(InputModalities::TextAndImage.supports_image());
    }

    #[test]
    fn upstream_capabilities_recognize_only_known_strings() {
        assert_eq!(
            UpstreamCapabilities::from_advertised(&["image", "reasoning"]),
            UpstreamCapabilities {
                image: true,
                reasoning: true
            }
        );
        assert_eq!(
            UpstreamCapabilities::from_advertised(&["vision", "multimodal", "IMAGE"]),
            UpstreamCapabilities::default(),
            "capability support must never be guessed from unrecognized strings"
        );
        assert_eq!(
            UpstreamCapabilities::from_advertised::<String>(&[]),
            UpstreamCapabilities::default()
        );
    }

    #[test]
    fn resolution_prefers_configuration_over_upstream_metadata() {
        let capabilities = ModelCapabilities::new(BTreeMap::from([
            ("vision-model".to_owned(), modalities(InputModalities::TextAndImage)),
            ("pinned-text-model".to_owned(), modalities(InputModalities::Text)),
        ]));
        let advertises_image = UpstreamCapabilities {
            image: true,
            reasoning: false,
        };
        let advertises_nothing = UpstreamCapabilities::default();

        assert_eq!(
            capabilities.resolve("vision-model", advertises_nothing),
            InputModalities::TextAndImage,
            "a configured vision model must advertise images without upstream metadata"
        );
        assert_eq!(
            capabilities.resolve("pinned-text-model", advertises_image),
            InputModalities::Text,
            "an explicit text-only override must win over upstream image metadata"
        );
        assert_eq!(
            capabilities.resolve("unconfigured-model", advertises_image),
            InputModalities::TextAndImage,
            "recognized upstream metadata must be used when no override exists"
        );
        assert_eq!(
            capabilities.resolve("unconfigured-model", advertises_nothing),
            InputModalities::Text,
            "an unknown model without metadata must stay text-only"
        );
        assert_eq!(
            ModelCapabilities::default().resolve("vision-model", advertises_nothing),
            InputModalities::Text
        );
    }

    #[test]
    fn resolution_keeps_configured_reasoning_levels_and_defaults_the_rest() {
        let capabilities = ModelCapabilities::new(BTreeMap::from([(
            "glm-model".to_owned(),
            levels(&[
                ReasoningEffortLevel::Low,
                ReasoningEffortLevel::High,
                ReasoningEffortLevel::Max,
            ]),
        )]));

        assert_eq!(
            capabilities.resolve_reasoning_levels("glm-model"),
            [
                ReasoningEffortLevel::Low,
                ReasoningEffortLevel::High,
                ReasoningEffortLevel::Max
            ],
            "configured levels must be advertised in the configured order"
        );
        assert_eq!(
            capabilities.resolve_reasoning_levels("unconfigured-model"),
            DEFAULT_REASONING_LEVELS
        );
        assert_eq!(
            ModelCapabilities::default().resolve_reasoning_levels("glm-model"),
            DEFAULT_REASONING_LEVELS
        );
    }

    #[test]
    fn catalog_selection_matches_slugs_exactly() {
        let catalog: CodexCatalogCapabilities = serde_json::from_str(
            r#"{"models":[
                {"slug":"first-model","input_modalities":["text"],"display_name":"ignored"},
                {"slug":"vision-model","input_modalities":["text","image"]}
            ]}"#,
        )
        .expect("parse catalog");

        assert_eq!(catalog.select(None).expect("first entry").slug, "first-model");
        let selected = catalog.select(Some("vision-model")).expect("selected entry");
        assert_eq!(selected.input_modalities, InputModalities::TextAndImage);
        assert!(
            catalog.select(Some("Vision-Model")).is_none(),
            "slugs are case-sensitive"
        );
        assert!(catalog.select(Some("missing-model")).is_none());
    }

    #[test]
    fn catalog_without_models_selects_nothing() {
        let catalog: CodexCatalogCapabilities = serde_json::from_str("{}").expect("parse empty catalog");

        assert!(catalog.models.is_empty());
        assert!(catalog.select(None).is_none());
        assert!(catalog.select(Some("any-model")).is_none());
    }
}

/// Model metadata obtained from the connected native engine.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ServedModel {
    pub id: String,
    pub max_model_len: u32,
    object: &'static str,
    created: u64,
    owned_by: &'static str,
}

impl ServedModel {
    #[must_use]
    pub fn new(id: String, max_model_len: u32) -> Self {
        Self {
            id,
            max_model_len,
            object: "model",
            created: 0,
            owned_by: "vllm",
        }
    }
}
