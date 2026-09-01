//! A validated, repeatable recipe for rebuilding one private runtime.
//!
//! The provider factory is the only moving part. Everything else was already
//! a value or Arc-backed policy on [`RuntimeBuilder`]; conversion rejects the
//! three ownership shapes that cannot be replayed honestly: concrete provider
//! instances, concrete host tools, and non-ephemeral history. Each build makes
//! one provider, takes one ordinary clone for the host's warm step, installs
//! the other in Mentra, finishes the runtime, and only then invokes and awaits
//! warming. There is never a half-warmed runtime for a caller to retain.

use std::{
    error::Error,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use crate::{error::RunError, shell::ShellAccess};

use super::{
    Runtime,
    builder::{HostProvider, RuntimeBuilder},
};

type BoxRecipeError = Box<dyn Error + Send + Sync + 'static>;
type WarmFuture = Pin<Box<dyn Future<Output = Result<(), BoxRecipeError>> + Send + 'static>>;
type WarmOnce = Box<dyn FnOnce() -> WarmFuture + Send + 'static>;

struct ProviderGeneration {
    host_provider: HostProvider,
    warm: WarmOnce,
}

/// Type-erased repeatable provider generation retained by a runtime recipe.
pub(super) struct ReusableProvider {
    id: mentra::ProviderId,
    make: Arc<dyn Fn() -> Result<ProviderGeneration, BoxRecipeError> + Send + Sync + 'static>,
}

impl ReusableProvider {
    pub(super) fn new<P, Make, MakeError, Warm, WarmOutput, WarmError>(
        id: mentra::ProviderId,
        make: Make,
        warm: Warm,
    ) -> Self
    where
        P: mentra::provider_core::Provider + Clone + 'static,
        Make: Fn() -> Result<P, MakeError> + Send + Sync + 'static,
        MakeError: Error + Send + Sync + 'static,
        Warm: Fn(P) -> WarmOutput + Send + Sync + 'static,
        WarmOutput: Future<Output = Result<(), WarmError>> + Send + 'static,
        WarmError: Error + Send + Sync + 'static,
    {
        let warm = Arc::new(warm);
        Self {
            id,
            make: Arc::new(move || {
                let provider = make().map_err(boxed)?;
                let warm_provider = provider.clone();
                let host_provider = HostProvider::registered(provider);
                let warm = Arc::clone(&warm);

                Ok(ProviderGeneration {
                    host_provider,
                    // Calling `warm` itself may have synchronous effects, so
                    // defer the call rather than only deferring its future.
                    warm: Box::new(move || {
                        Box::pin(async move { warm(warm_provider).await.map_err(boxed) })
                    }),
                })
            }),
        }
    }

    fn generate(&self) -> Result<ProviderGeneration, RunError> {
        let generation = (self.make)().map_err(RunError::RuntimeRecipeProviderFactory)?;
        if generation.host_provider.id() != &self.id {
            return Err(RunError::RuntimeRecipeProviderMismatch {
                declared: self.id.as_str().to_string(),
                generated: generation.host_provider.id().as_str().to_string(),
            });
        }
        Ok(generation)
    }

    fn id(&self) -> &mentra::ProviderId {
        &self.id
    }
}

fn boxed<E>(error: E) -> BoxRecipeError
where
    E: Error + Send + Sync + 'static,
{
    Box::new(error)
}

/// A validated provider/runtime recipe that can build a fresh private runtime
/// more than once.
///
/// Construct this with [`RuntimeBuilder::into_reusable_recipe`]. The recipe
/// itself does not expose a public raw-runtime constructor: a reusable runtime
/// must enter through the workspace lifecycle that owns its exclusive-use and
/// teardown contract. Basis uses the crate-private builders below for that
/// integration.
pub struct RuntimeRecipe {
    template: RuntimeBuilder,
    provider: ReusableProvider,
}

impl std::fmt::Debug for RuntimeRecipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeRecipe")
            .field("template", &self.template)
            .field("provider", &self.provider.id().as_str())
            .finish()
    }
}

impl RuntimeRecipe {
    pub(super) fn new(template: RuntimeBuilder, provider: ReusableProvider) -> Self {
        Self { template, provider }
    }

    pub(crate) fn provider(&self) -> &mentra::ProviderId {
        self.provider.id()
    }

    /// Builds the workspace-agnostic form used by focused recipe tests.
    ///
    /// The production consumer is [`build_for`](Self::build_for); keeping this
    /// crate-private avoids offering a raw reusable runtime outside the owner
    /// that can prove it was consumed before rebuilding.
    #[allow(dead_code)]
    pub(crate) async fn build(&self) -> Result<Runtime, RunError> {
        self.build_with(RuntimeBuilder::build).await
    }

    /// Builds one private runtime bound to `workspace` from this recipe.
    #[allow(dead_code)]
    pub(crate) async fn build_for(
        &self,
        workspace: &Path,
        shell: ShellAccess,
        memory_roots: &[PathBuf],
    ) -> Result<Runtime, RunError> {
        self.build_with(|builder| builder.build_for(workspace, shell, memory_roots))
            .await
    }

    async fn build_with(
        &self,
        build: impl FnOnce(RuntimeBuilder) -> Result<Runtime, RunError>,
    ) -> Result<Runtime, RunError> {
        let ProviderGeneration {
            host_provider,
            warm,
        } = self.provider.generate()?;

        // Build must finish before even invoking the host closure. Besides
        // preserving the stated order, this guarantees invalid policy or
        // runtime assembly cannot cause a connection side effect.
        let runtime = build(self.template.replay_with_host_provider(host_provider))?;
        warm().await.map_err(RunError::RuntimeRecipeProviderWarm)?;
        Ok(runtime)
    }
}
