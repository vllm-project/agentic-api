//! Operator-configured model capabilities shared by request admission and model catalogs.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{ReasoningEffortLevel, ServiceTier};

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
    /// Explicit reasoning levels shared by catalog display and request admission.
    /// `None` uses catalog display defaults without restricting requests.
    pub supported_reasoning_levels: Option<SupportedReasoningLevels>,
    /// Whether requests may set `temperature` or `top_p`.
    pub sampling_supported: Option<bool>,
    /// Whether requests may set `top_logprobs` or include output-text logprobs.
    pub logprobs_supported: Option<bool>,
    /// Accepted explicit service tiers, or `None` to leave the decision to the upstream.
    pub service_tiers: Option<Vec<ServiceTier>>,
    /// Overrides the deployment-wide cache mode/TTL assertion.
    pub prompt_cache_options_supported: Option<bool>,
    /// Whether requests may set the legacy retention field.
    pub prompt_cache_retention_supported: Option<bool>,
}

/// Local capability overrides, keyed by served model ID.
#[derive(Clone, Debug, Default)]
pub struct ModelCapabilities {
    // Config is cloned by transport state extraction; keep this immutable table shared with the executor.
    overrides: Arc<BTreeMap<String, ModelOverride>>,
}

impl ModelCapabilities {
    /// Build a resolver from configured per-model overrides.
    #[must_use]
    pub fn new(overrides: BTreeMap<String, ModelOverride>) -> Self {
        Self {
            overrides: Arc::new(overrides),
        }
    }

    /// The explicitly configured override for this exact, case-sensitive served model ID.
    ///
    /// Admission uses this lookup rather than catalog defaults, which are only presentation fallbacks.
    #[must_use]
    pub fn for_model(&self, model_id: &str) -> Option<&ModelOverride> {
        self.overrides.get(model_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_defaults_do_not_create_admission_overrides() {
        let capabilities = ModelCapabilities::new(BTreeMap::from([(
            "example".to_owned(),
            ModelOverride {
                sampling_supported: Some(false),
                ..ModelOverride::default()
            },
        )]));
        assert_eq!(
            capabilities.resolve_reasoning_levels("example"),
            DEFAULT_REASONING_LEVELS
        );
        assert!(
            capabilities
                .for_model("example")
                .unwrap()
                .supported_reasoning_levels
                .is_none()
        );
        assert!(capabilities.for_model("other").is_none());
        assert!(capabilities.for_model("EXAMPLE").is_none());
        assert_eq!(capabilities.resolve_reasoning_levels("other"), DEFAULT_REASONING_LEVELS);
    }
}
