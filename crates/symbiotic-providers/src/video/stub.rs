//! Stub video providers for services not yet integrated.
//!
//! Each stub returns [`ProviderError::Unavailable`] for all video generation
//! requests. Pre-configured factory functions are provided for known services.

use async_trait::async_trait;

use crate::{
    CapabilitySet, ModelProvider, PricingInfo, ProviderCapability, ProviderClass, ProviderError,
    VideoProvider, VideoRequest, VideoResponse,
};

/// A placeholder video provider that always returns [`ProviderError::Unavailable`].
///
/// Used to register future video providers (Higgsfield, Runway, Kling,
/// Stable Video) in the provider registry before their APIs are integrated.
pub struct StubVideoProvider {
    name: String,
    provider_class: ProviderClass,
    model: String,
    capabilities: CapabilitySet,
}

impl StubVideoProvider {
    /// Create a new stub video provider with the given metadata.
    pub fn new(
        name: impl Into<String>,
        provider_class: ProviderClass,
        model: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            provider_class,
            model: model.into(),
            capabilities: CapabilitySet::new(vec![ProviderCapability::VideoGeneration]),
        }
    }

    /// Stub for the Higgsfield video service (cloud).
    pub fn higgsfield() -> Self {
        Self::new("higgsfield", ProviderClass::Cloud, "higgsfield-v1")
    }

    /// Stub for the Runway video service (cloud).
    pub fn runway() -> Self {
        Self::new("runway", ProviderClass::Cloud, "gen-3-alpha")
    }

    /// Stub for the Kling video service (cloud).
    pub fn kling() -> Self {
        Self::new("kling", ProviderClass::Cloud, "kling-v1")
    }

    /// Stub for Stable Video Diffusion (local).
    pub fn stable_video() -> Self {
        Self::new("stable-video", ProviderClass::Local, "svd-xt")
    }
}

impl ModelProvider for StubVideoProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn provider_class(&self) -> ProviderClass {
        self.provider_class
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        None
    }
}

#[async_trait]
impl VideoProvider for StubVideoProvider {
    async fn generate_video(
        &self,
        _request: &VideoRequest,
    ) -> Result<VideoResponse, ProviderError> {
        Err(ProviderError::Unavailable(format!(
            "{} video provider not yet implemented",
            self.name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_metadata() {
        let hf = StubVideoProvider::higgsfield();
        assert_eq!(hf.name(), "higgsfield");
        assert_eq!(hf.provider_class(), ProviderClass::Cloud);
        assert_eq!(hf.model_name(), "higgsfield-v1");
        assert!(hf.capabilities().has(ProviderCapability::VideoGeneration));
        assert!(hf.pricing().is_none());

        let rw = StubVideoProvider::runway();
        assert_eq!(rw.name(), "runway");
        assert_eq!(rw.model_name(), "gen-3-alpha");

        let kl = StubVideoProvider::kling();
        assert_eq!(kl.name(), "kling");

        let sv = StubVideoProvider::stable_video();
        assert_eq!(sv.name(), "stable-video");
        assert_eq!(sv.provider_class(), ProviderClass::Local);
    }

    #[tokio::test]
    async fn stub_returns_unavailable() {
        let provider = StubVideoProvider::runway();
        let request = VideoRequest {
            prompt: "a test video".into(),
            duration_seconds: None,
            width: None,
            height: None,
        };
        let err = provider.generate_video(&request).await.unwrap_err();
        match err {
            ProviderError::Unavailable(msg) => {
                assert!(
                    msg.contains("runway"),
                    "expected provider name in message: {msg}"
                );
                assert!(
                    msg.contains("not yet implemented"),
                    "expected 'not yet implemented': {msg}"
                );
            }
            other => panic!("expected Unavailable, got: {other}"),
        }
    }
}
