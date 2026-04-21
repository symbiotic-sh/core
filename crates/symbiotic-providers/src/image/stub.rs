//! Stub image providers for services not yet integrated.
//!
//! Each stub returns [`ProviderError::Unavailable`] for all image generation
//! requests. Pre-configured factory functions are provided for known services.

use async_trait::async_trait;

use crate::{
    CapabilitySet, ImageProvider, ImageRequest, ImageResponse, ModelProvider, PricingInfo,
    ProviderCapability, ProviderClass, ProviderError,
};

/// A placeholder image provider that always returns [`ProviderError::Unavailable`].
///
/// Used to register future image providers (Flux, Stable Diffusion, nanobanana,
/// Midjourney) in the provider registry before their APIs are integrated.
pub struct StubImageProvider {
    name: String,
    provider_class: ProviderClass,
    model: String,
    capabilities: CapabilitySet,
}

impl StubImageProvider {
    /// Create a new stub image provider with the given metadata.
    pub fn new(
        name: impl Into<String>,
        provider_class: ProviderClass,
        model: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            provider_class,
            model: model.into(),
            capabilities: CapabilitySet::new(vec![ProviderCapability::ImageGeneration]),
        }
    }

    /// Stub for the nanobanana image service (cloud).
    pub fn nanobanana() -> Self {
        Self::new("nanobanana", ProviderClass::Cloud, "nanobanana-v1")
    }

    /// Stub for the Flux image service (cloud).
    pub fn flux() -> Self {
        Self::new("flux", ProviderClass::Cloud, "flux-1")
    }

    /// Stub for Stable Diffusion (local).
    pub fn stable_diffusion() -> Self {
        Self::new("stable-diffusion", ProviderClass::Local, "sd-xl")
    }

    /// Stub for Midjourney (cloud).
    pub fn midjourney() -> Self {
        Self::new("midjourney", ProviderClass::Cloud, "midjourney-v6")
    }
}

impl ModelProvider for StubImageProvider {
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
impl ImageProvider for StubImageProvider {
    async fn generate_image(
        &self,
        _request: &ImageRequest,
    ) -> Result<ImageResponse, ProviderError> {
        Err(ProviderError::Unavailable(format!(
            "{} image provider not yet implemented",
            self.name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_metadata() {
        let nb = StubImageProvider::nanobanana();
        assert_eq!(nb.name(), "nanobanana");
        assert_eq!(nb.provider_class(), ProviderClass::Cloud);
        assert_eq!(nb.model_name(), "nanobanana-v1");
        assert!(nb.capabilities().has(ProviderCapability::ImageGeneration));
        assert!(nb.pricing().is_none());

        let flux = StubImageProvider::flux();
        assert_eq!(flux.name(), "flux");

        let sd = StubImageProvider::stable_diffusion();
        assert_eq!(sd.name(), "stable-diffusion");
        assert_eq!(sd.provider_class(), ProviderClass::Local);

        let mj = StubImageProvider::midjourney();
        assert_eq!(mj.name(), "midjourney");
    }

    #[tokio::test]
    async fn stub_returns_unavailable() {
        let provider = StubImageProvider::nanobanana();
        let request = ImageRequest {
            prompt: "a test image".into(),
            width: None,
            height: None,
            count: None,
        };
        let err = provider.generate_image(&request).await.unwrap_err();
        match err {
            ProviderError::Unavailable(msg) => {
                assert!(
                    msg.contains("nanobanana"),
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
