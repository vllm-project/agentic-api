//! Model capabilities shared by the Codex catalog handler, the gateway configuration loader,
//! and the harness launcher.
//!
//! Codex decides which reasoning effort levels to offer and whether image content may be sent
//! from its local model catalog, so the catalog the gateway serves and the isolated catalog a
//! launcher writes must not disagree about a served model. This module owns the gateway-side
//! contract: the validated modality set, the validated reasoning-level list, the upstream
//! metadata the gateway recognizes, the local override table, and the resolution order between
//! them.

use std::borrow::Cow;
use std::collections::BTreeMap;

use agentic_core::types::messages::ReasoningEffortLevel;
use serde::{Deserialize, Serialize};

/// Upstream capability string recognized as image input support.
const IMAGE_CAPABILITY: &str = "image";
/// Upstream capability string recognized as reasoning support.
const REASONING_CAPABILITY: &str = "reasoning";

/// One input modality a served model accepts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Text input, which every model served through the gateway accepts.
    Text,
    /// Image input.
    Image,
}

impl Modality {
    /// The wire representation of this modality.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
        }
    }
}

/// Rejected input-modality configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModalityError {
    /// The list held no modalities.
    #[error("input_modalities must list at least one modality; expected [\"text\"] or [\"text\", \"image\"]")]
    Empty,
    /// The list repeated a modality.
    #[error("input_modalities lists \"{}\" more than once", .0.as_str())]
    Duplicate(Modality),
    /// The list omitted text.
    #[error("input_modalities must include \"text\"; a coding harness cannot use an image-only model")]
    MissingText,
}

/// The validated input modalities advertised for one served model.
///
/// A coding harness always sends instructions, history, and tool output as text, so text is
/// mandatory and the only usable combinations are text and text-with-image. Every other
/// combination is rejected while parsing, which keeps empty, duplicated, image-only, and
/// misordered states unrepresentable.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "Vec<Modality>", into = "Vec<Modality>")]
pub enum InputModalities {
    /// Text input only: the conservative fallback when nothing advertises image support.
    #[default]
    Text,
    /// Text and image input.
    TextAndImage,
}

impl InputModalities {
    /// The modalities in the canonical wire order.
    #[must_use]
    pub const fn as_slice(self) -> &'static [Modality] {
        match self {
            Self::Text => &[Modality::Text],
            Self::TextAndImage => &[Modality::Text, Modality::Image],
        }
    }

    /// Whether `modality` is accepted.
    #[must_use]
    pub const fn contains(self, modality: Modality) -> bool {
        matches!(
            (self, modality),
            (Self::Text | Self::TextAndImage, Modality::Text) | (Self::TextAndImage, Modality::Image)
        )
    }

    /// Whether image input is accepted.
    #[must_use]
    pub const fn supports_image(self) -> bool {
        matches!(self, Self::TextAndImage)
    }

    /// Validate an unordered modality list.
    ///
    /// # Errors
    ///
    /// Returns [`ModalityError`] when the list is empty, repeats a modality, or omits text.
    pub fn try_new(values: &[Modality]) -> Result<Self, ModalityError> {
        let mut text = false;
        let mut image = false;
        for value in values {
            let seen = match value {
                Modality::Text => &mut text,
                Modality::Image => &mut image,
            };
            if *seen {
                return Err(ModalityError::Duplicate(*value));
            }
            *seen = true;
        }
        match (text, image) {
            (true, true) => Ok(Self::TextAndImage),
            (true, false) => Ok(Self::Text),
            (false, true) => Err(ModalityError::MissingText),
            (false, false) => Err(ModalityError::Empty),
        }
    }
}

impl TryFrom<Vec<Modality>> for InputModalities {
    type Error = ModalityError;

    fn try_from(values: Vec<Modality>) -> Result<Self, Self::Error> {
        Self::try_new(&values)
    }
}

impl From<InputModalities> for Vec<Modality> {
    fn from(modalities: InputModalities) -> Self {
        modalities.as_slice().to_vec()
    }
}

/// Rejected reasoning-level configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReasoningLevelsError {
    /// The list held no levels.
    #[error("supported_reasoning_levels must list at least one level; expected [\"low\", \"medium\", \"high\"]")]
    Empty,
    /// The list repeated a level.
    #[error("supported_reasoning_levels lists \"{}\" more than once", .0.as_str())]
    Duplicate(ReasoningEffortLevel),
}

/// The reasoning effort levels Codex offers for a model that configures none.
pub const DEFAULT_REASONING_LEVELS: &[ReasoningEffortLevel] = &[
    ReasoningEffortLevel::Low,
    ReasoningEffortLevel::Medium,
    ReasoningEffortLevel::High,
];

/// The validated reasoning effort levels advertised for one served model.
///
/// Declaration order is preserved, because Codex lists the levels in catalog order. An empty
/// list and a repeated level are rejected while parsing, which keeps those states
/// unrepresentable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "Vec<ReasoningEffortLevel>", into = "Vec<ReasoningEffortLevel>")]
pub struct SupportedReasoningLevels(Vec<ReasoningEffortLevel>);

impl SupportedReasoningLevels {
    /// The levels in configured order.
    #[must_use]
    pub fn as_slice(&self) -> &[ReasoningEffortLevel] {
        &self.0
    }

    /// Validate a level list.
    ///
    /// # Errors
    ///
    /// Returns [`ReasoningLevelsError`] when the list is empty or repeats a level.
    pub fn try_new(values: &[ReasoningEffortLevel]) -> Result<Self, ReasoningLevelsError> {
        let mut unique = Vec::with_capacity(values.len());
        for value in values {
            if unique.contains(value) {
                return Err(ReasoningLevelsError::Duplicate(*value));
            }
            unique.push(*value);
        }
        if unique.is_empty() {
            return Err(ReasoningLevelsError::Empty);
        }
        Ok(Self(unique))
    }
}

impl TryFrom<Vec<ReasoningEffortLevel>> for SupportedReasoningLevels {
    type Error = ReasoningLevelsError;

    fn try_from(values: Vec<ReasoningEffortLevel>) -> Result<Self, Self::Error> {
        Self::try_new(&values)
    }
}

impl From<SupportedReasoningLevels> for Vec<ReasoningEffortLevel> {
    fn from(levels: SupportedReasoningLevels) -> Self {
        levels.0
    }
}

/// Capabilities an upstream `/v1/models` entry advertises.
///
/// Unrecognized capability strings are ignored: the upstream vocabulary is vendor-defined and
/// may grow, and an unknown string is never evidence of a capability.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpstreamCapabilities {
    /// The entry advertises image input.
    pub image: bool,
    /// The entry advertises reasoning output.
    pub reasoning: bool,
}

impl UpstreamCapabilities {
    /// Recognize the capability strings an upstream model entry advertises.
    #[must_use]
    pub fn from_advertised<S: AsRef<str>>(entries: &[S]) -> Self {
        entries
            .iter()
            .fold(Self::default(), |capabilities, entry| match entry.as_ref() {
                IMAGE_CAPABILITY => Self {
                    image: true,
                    ..capabilities
                },
                REASONING_CAPABILITY => Self {
                    reasoning: true,
                    ..capabilities
                },
                _ => capabilities,
            })
    }
}

/// Capability overrides configured for one served model.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelOverride {
    /// Input modalities to advertise, or `None` to infer them.
    pub input_modalities: Option<InputModalities>,
    /// Reasoning effort levels to advertise in catalog order, or `None` for the default.
    pub supported_reasoning_levels: Option<SupportedReasoningLevels>,
}

/// Local capability overrides, keyed by served model ID.
#[derive(Clone, Debug, Default)]
pub struct ModelCapabilities {
    overrides: BTreeMap<String, ModelOverride>,
}

impl ModelCapabilities {
    /// Build a resolver from configured per-model overrides.
    #[must_use]
    pub fn new(overrides: BTreeMap<String, ModelOverride>) -> Self {
        Self { overrides }
    }

    /// Resolve the input modalities advertised for `model_id`.
    ///
    /// Precedence is an explicit local override, then recognized upstream metadata, then the
    /// conservative text-only fallback. An explicit text-only override therefore wins over
    /// upstream image metadata, and a model with neither an override nor metadata stays
    /// text-only rather than being guessed from its name.
    #[must_use]
    pub fn resolve(&self, model_id: &str, upstream: UpstreamCapabilities) -> InputModalities {
        if let Some(modalities) = self
            .overrides
            .get(model_id)
            .and_then(|overrides| overrides.input_modalities)
        {
            return modalities;
        }
        if upstream.image {
            InputModalities::TextAndImage
        } else {
            InputModalities::Text
        }
    }

    /// Resolve the reasoning effort levels advertised for `model_id`, in catalog order.
    ///
    /// Unlike input modalities, levels are not inferred from upstream metadata: a model with no
    /// override keeps [`DEFAULT_REASONING_LEVELS`].
    #[must_use]
    pub fn resolve_reasoning_levels(&self, model_id: &str) -> &[ReasoningEffortLevel] {
        self.overrides
            .get(model_id)
            .and_then(|overrides| overrides.supported_reasoning_levels.as_ref())
            .map_or(DEFAULT_REASONING_LEVELS, SupportedReasoningLevels::as_slice)
    }
}

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
