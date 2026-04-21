//! Provider registry for managing registered AI providers.
//!
//! The [`ProviderRegistry`] holds a set of [`RegisteredProvider`] entries,
//! each wrapping optional trait objects per modality. Providers are keyed
//! by name and can be looked up by name, capability, or class. A separate
//! defaults map specifies which provider to use for each capability when
//! the caller does not express a preference.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::ProviderError;
use crate::traits::*;
use crate::types::*;

/// A provider entry in the registry, holding optional trait objects per modality.
///
/// All fields are `Arc`-wrapped, so cloning is cheap (reference count bump).
pub struct RegisteredProvider {
    /// Base provider metadata.
    pub base: Arc<dyn ModelProvider>,
    /// Completion interface (if supported).
    pub completion: Option<Arc<dyn CompletionProvider>>,
    /// Embedding interface (if supported).
    pub embedding: Option<Arc<dyn EmbeddingProvider>>,
    /// Image generation interface (if supported).
    pub image: Option<Arc<dyn ImageProvider>>,
    /// Video generation interface (if supported).
    pub video: Option<Arc<dyn VideoProvider>>,
    /// Agent execution interface (if supported).
    pub agent: Option<Arc<dyn AgentProvider>>,
}

impl Clone for RegisteredProvider {
    fn clone(&self) -> Self {
        Self {
            base: Arc::clone(&self.base),
            completion: self.completion.as_ref().map(Arc::clone),
            embedding: self.embedding.as_ref().map(Arc::clone),
            image: self.image.as_ref().map(Arc::clone),
            video: self.video.as_ref().map(Arc::clone),
            agent: self.agent.as_ref().map(Arc::clone),
        }
    }
}

/// Central registry of all configured AI providers.
///
/// Providers are registered with [`ProviderRegistry::register`] and indexed
/// by name. The registry supports lookup by name, capability, or class, and
/// maintains a defaults map for capability-based routing.
pub struct ProviderRegistry {
    providers: HashMap<String, RegisteredProvider>,
    defaults: HashMap<ProviderCapability, String>,
}

impl ProviderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            defaults: HashMap::new(),
        }
    }

    /// Register a provider. Uses `base.name()` as the key.
    ///
    /// If a provider with the same name already exists it is replaced.
    pub fn register(&mut self, provider: RegisteredProvider) {
        let name = provider.base.name().to_string();
        self.providers.insert(name, provider);
    }

    /// Set the default provider for a given capability.
    ///
    /// Returns an error if the named provider is not registered.
    pub fn set_default(
        &mut self,
        capability: ProviderCapability,
        provider_name: &str,
    ) -> Result<(), ProviderError> {
        if !self.providers.contains_key(provider_name) {
            return Err(ProviderError::ConfigError(format!(
                "cannot set default: provider '{}' not registered",
                provider_name
            )));
        }
        self.defaults.insert(capability, provider_name.to_string());
        Ok(())
    }

    /// Look up a registered provider by name.
    pub fn get(&self, name: &str) -> Option<&RegisteredProvider> {
        self.providers.get(name)
    }

    /// Return all providers that advertise a given capability.
    pub fn by_capability(&self, cap: ProviderCapability) -> Vec<&RegisteredProvider> {
        self.providers
            .values()
            .filter(|p| p.base.capabilities().has(cap))
            .collect()
    }

    /// Return all providers that belong to a given class (local, cloud, aggregator).
    pub fn by_class(&self, class: ProviderClass) -> Vec<&RegisteredProvider> {
        self.providers
            .values()
            .filter(|p| p.base.provider_class() == class)
            .collect()
    }

    /// Get the default provider for a capability, if one has been configured.
    pub fn default_for(&self, cap: ProviderCapability) -> Option<&RegisteredProvider> {
        self.defaults
            .get(&cap)
            .and_then(|name| self.providers.get(name))
    }

    /// List the names of all registered providers.
    pub fn names(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Mock provider for testing ----------------------------------------------

    struct MockProvider {
        name: String,
        class: ProviderClass,
        model: String,
        capabilities: CapabilitySet,
    }

    impl MockProvider {
        fn new(name: &str, class: ProviderClass, caps: Vec<ProviderCapability>) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: format!("{name}-model"),
                capabilities: CapabilitySet::new(caps),
            }
        }
    }

    impl ModelProvider for MockProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn provider_class(&self) -> ProviderClass {
            self.class
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

    fn make_entry(
        name: &str,
        class: ProviderClass,
        caps: Vec<ProviderCapability>,
    ) -> RegisteredProvider {
        RegisteredProvider {
            base: Arc::new(MockProvider::new(name, class, caps)),
            completion: None,
            embedding: None,
            image: None,
            video: None,
            agent: None,
        }
    }

    // -- Tests ------------------------------------------------------------------

    #[test]
    fn register_and_get_by_name() {
        let mut reg = ProviderRegistry::new();
        reg.register(make_entry(
            "ollama",
            ProviderClass::Local,
            vec![
                ProviderCapability::Completion,
                ProviderCapability::Embedding,
            ],
        ));
        reg.register(make_entry(
            "openai",
            ProviderClass::Cloud,
            vec![
                ProviderCapability::Completion,
                ProviderCapability::ImageGeneration,
            ],
        ));

        assert!(reg.get("ollama").is_some());
        assert_eq!(reg.get("ollama").unwrap().base.name(), "ollama");
        assert!(reg.get("openai").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn by_capability_filters_correctly() {
        let mut reg = ProviderRegistry::new();
        reg.register(make_entry(
            "ollama",
            ProviderClass::Local,
            vec![
                ProviderCapability::Completion,
                ProviderCapability::Embedding,
            ],
        ));
        reg.register(make_entry(
            "openai",
            ProviderClass::Cloud,
            vec![
                ProviderCapability::Completion,
                ProviderCapability::ImageGeneration,
            ],
        ));
        reg.register(make_entry(
            "flux",
            ProviderClass::Cloud,
            vec![ProviderCapability::ImageGeneration],
        ));

        let completers = reg.by_capability(ProviderCapability::Completion);
        assert_eq!(completers.len(), 2);

        let embedders = reg.by_capability(ProviderCapability::Embedding);
        assert_eq!(embedders.len(), 1);
        assert_eq!(embedders[0].base.name(), "ollama");

        let imagers = reg.by_capability(ProviderCapability::ImageGeneration);
        assert_eq!(imagers.len(), 2);

        let video = reg.by_capability(ProviderCapability::VideoGeneration);
        assert!(video.is_empty());
    }

    #[test]
    fn by_class_filters_correctly() {
        let mut reg = ProviderRegistry::new();
        reg.register(make_entry(
            "ollama",
            ProviderClass::Local,
            vec![ProviderCapability::Completion],
        ));
        reg.register(make_entry(
            "openai",
            ProviderClass::Cloud,
            vec![ProviderCapability::Completion],
        ));
        reg.register(make_entry(
            "openrouter",
            ProviderClass::Aggregator,
            vec![ProviderCapability::Completion],
        ));

        let local = reg.by_class(ProviderClass::Local);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].base.name(), "ollama");

        let cloud = reg.by_class(ProviderClass::Cloud);
        assert_eq!(cloud.len(), 1);
        assert_eq!(cloud[0].base.name(), "openai");

        let agg = reg.by_class(ProviderClass::Aggregator);
        assert_eq!(agg.len(), 1);
        assert_eq!(agg[0].base.name(), "openrouter");
    }

    #[test]
    fn default_for_returns_correct_provider() {
        let mut reg = ProviderRegistry::new();
        reg.register(make_entry(
            "ollama",
            ProviderClass::Local,
            vec![ProviderCapability::Completion],
        ));
        reg.register(make_entry(
            "openai",
            ProviderClass::Cloud,
            vec![ProviderCapability::Completion],
        ));

        // No default set yet.
        assert!(reg.default_for(ProviderCapability::Completion).is_none());

        // Set default.
        reg.set_default(ProviderCapability::Completion, "ollama")
            .unwrap();
        let default = reg.default_for(ProviderCapability::Completion).unwrap();
        assert_eq!(default.base.name(), "ollama");

        // Override default.
        reg.set_default(ProviderCapability::Completion, "openai")
            .unwrap();
        let default = reg.default_for(ProviderCapability::Completion).unwrap();
        assert_eq!(default.base.name(), "openai");
    }

    #[test]
    fn set_default_rejects_unknown_provider() {
        let mut reg = ProviderRegistry::new();
        let result = reg.set_default(ProviderCapability::Completion, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn names_lists_all_providers() {
        let mut reg = ProviderRegistry::new();
        reg.register(make_entry(
            "ollama",
            ProviderClass::Local,
            vec![ProviderCapability::Completion],
        ));
        reg.register(make_entry(
            "openai",
            ProviderClass::Cloud,
            vec![ProviderCapability::Completion],
        ));
        reg.register(make_entry(
            "openrouter",
            ProviderClass::Aggregator,
            vec![ProviderCapability::Completion],
        ));

        let mut names = reg.names();
        names.sort();
        assert_eq!(names, vec!["ollama", "openai", "openrouter"]);
    }

    #[test]
    fn register_replaces_existing_provider() {
        let mut reg = ProviderRegistry::new();
        reg.register(make_entry(
            "ollama",
            ProviderClass::Local,
            vec![ProviderCapability::Completion],
        ));
        assert!(!reg
            .get("ollama")
            .unwrap()
            .base
            .capabilities()
            .has(ProviderCapability::Embedding));

        // Register again with different capabilities.
        reg.register(make_entry(
            "ollama",
            ProviderClass::Local,
            vec![
                ProviderCapability::Completion,
                ProviderCapability::Embedding,
            ],
        ));
        assert!(reg
            .get("ollama")
            .unwrap()
            .base
            .capabilities()
            .has(ProviderCapability::Embedding));
        assert_eq!(reg.names().len(), 1);
    }

    #[test]
    fn default_impl_creates_empty_registry() {
        let reg = ProviderRegistry::default();
        assert!(reg.names().is_empty());
    }
}
