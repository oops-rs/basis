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

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc, Mutex, Weak,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use mentra::provider_core::{
        Provider as _, ProviderDefinition, ProviderDescriptor, ProviderError, ProviderId,
        ProviderSession, ProviderSessionFactory, StaticCredentialSource,
        responses::ResponsesProvider,
    };

    use crate::{RunError, RuntimeBuilder};

    type TestProvider = ResponsesProvider<StaticCredentialSource>;

    fn provider(id: impl Into<String>) -> TestProvider {
        let mut definition = mentra::provider_core::responses::openai_definition();
        definition.descriptor.id = ProviderId::new(id);
        definition.base_url = Some("http://127.0.0.1:1/".to_string());
        ResponsesProvider::new(definition, StaticCredentialSource::new("test-key"))
    }

    fn reusable(builder: RuntimeBuilder) -> RuntimeBuilder {
        builder.with_reusable_registered_provider(
            "repeatable",
            || Ok::<_, io::Error>(provider("repeatable")),
            |_provider| async { Ok::<_, io::Error>(()) },
        )
    }

    fn recipe(builder: RuntimeBuilder) -> super::RuntimeRecipe {
        reusable(builder)
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect("the repeatable provider and ephemeral store form a recipe")
    }

    #[test]
    fn conversion_requires_explicit_ephemeral_history() {
        for builder in [
            reusable(RuntimeBuilder::default()),
            reusable(RuntimeBuilder::default().with_store_dir("/tmp/basis-recipe-durable")),
        ] {
            let error = builder
                .into_reusable_recipe()
                .expect_err("durable or implicit history cannot be scrubbed by rebuilding");
            assert!(matches!(
                error,
                RunError::NonReusableRuntimeComponent {
                    component: "history"
                }
            ));
        }
    }

    #[test]
    fn conversion_rejects_a_one_shot_host_tool() {
        let error = reusable(RuntimeBuilder::default())
            .with_tool(crate::tools::SpawnTool::new())
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect_err("a moved host tool has no honest second instance");
        assert!(matches!(
            error,
            RunError::NonReusableRuntimeComponent {
                component: "host tools"
            }
        ));
    }

    #[test]
    fn the_last_host_provider_call_decides_reusability() {
        let one_shot_last = reusable(RuntimeBuilder::default())
            .with_registered_provider(provider("one-shot"))
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect_err("the final provider answer is one-shot");
        assert!(matches!(
            one_shot_last,
            RunError::NonReusableRuntimeComponent {
                component: "provider"
            }
        ));

        RuntimeBuilder::default()
            .with_registered_provider(provider("replaced"))
            .with_reusable_registered_provider(
                "repeatable",
                || Ok::<_, io::Error>(provider("repeatable")),
                |_provider| async { Ok::<_, io::Error>(()) },
            )
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect("the repeatable provider was the final answer");
    }

    #[test]
    fn synchronous_build_refuses_to_silently_skip_the_warm_step() {
        let error = reusable(RuntimeBuilder::default())
            .with_ephemeral_history()
            .build()
            .expect_err("a synchronous build cannot honor async warming");
        assert!(matches!(
            error,
            RunError::ReusableProviderRequiresRuntimeRecipe
        ));
    }

    #[tokio::test]
    async fn every_build_gets_one_fresh_provider_and_warms_its_ordinary_clone() {
        let makes = Arc::new(AtomicUsize::new(0));
        let warmed = Arc::new(Mutex::new(Vec::new()));
        let make_count = Arc::clone(&makes);
        let warmed_ids = Arc::clone(&warmed);
        let recipe = RuntimeBuilder::default()
            .with_reusable_registered_provider(
                "repeatable",
                move || {
                    make_count.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, io::Error>(provider("repeatable"))
                },
                move |provider: TestProvider| {
                    let warmed_ids = Arc::clone(&warmed_ids);
                    async move {
                        warmed_ids
                            .lock()
                            .expect("warm recorder")
                            .push(provider.descriptor().id.to_string());
                        Ok::<_, io::Error>(())
                    }
                },
            )
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect("recipe");

        let first = recipe.build().await.expect("first runtime");
        assert_eq!(first.provider(), "repeatable");
        drop(first);
        let second = recipe.build().await.expect("second runtime");
        assert_eq!(second.provider(), "repeatable");

        assert_eq!(makes.load(Ordering::SeqCst), 2);
        assert_eq!(
            *warmed.lock().expect("warm recorder"),
            ["repeatable", "repeatable"]
        );
    }

    #[tokio::test]
    async fn a_factory_failure_never_calls_warm() {
        let warms = Arc::new(AtomicUsize::new(0));
        let warm_count = Arc::clone(&warms);
        let recipe = RuntimeBuilder::default()
            .with_reusable_registered_provider(
                "repeatable",
                || -> Result<TestProvider, io::Error> { Err(io::Error::other("factory refused")) },
                move |_provider| {
                    warm_count.fetch_add(1, Ordering::SeqCst);
                    async { Ok::<_, io::Error>(()) }
                },
            )
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect("recipe validation is separate from generation");

        let error = recipe
            .build()
            .await
            .expect_err("a failed generation has no runtime");
        assert!(matches!(error, RunError::RuntimeRecipeProviderFactory(_)));
        assert_eq!(warms.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_factory_cannot_change_the_declared_provider_identity() {
        let warms = Arc::new(AtomicUsize::new(0));
        let warm_count = Arc::clone(&warms);
        let recipe = RuntimeBuilder::default()
            .with_reusable_registered_provider(
                "declared",
                || Ok::<_, io::Error>(provider("generated")),
                move |_provider| {
                    warm_count.fetch_add(1, Ordering::SeqCst);
                    async { Ok::<_, io::Error>(()) }
                },
            )
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect("recipe validation does not call the factory");

        let error = recipe
            .build()
            .await
            .expect_err("a generation cannot change provider identity");
        assert!(matches!(
            error,
            RunError::RuntimeRecipeProviderMismatch {
                declared,
                generated,
            } if declared == "declared" && generated == "generated"
        ));
        assert_eq!(warms.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_build_failure_happens_before_the_warm_closure_is_called() {
        let warms = Arc::new(AtomicUsize::new(0));
        let warm_count = Arc::clone(&warms);
        let recipe = RuntimeBuilder::default()
            .with_command_target("not a valid target", crate::runtime::LocalRuntimeExecutor)
            .with_reusable_registered_provider(
                "repeatable",
                || Ok::<_, io::Error>(provider("repeatable")),
                move |_provider| {
                    warm_count.fetch_add(1, Ordering::SeqCst);
                    async { Ok::<_, io::Error>(()) }
                },
            )
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect("the replayable values form a recipe");

        let error = recipe
            .build()
            .await
            .expect_err("target validation prevents a runtime");
        assert!(matches!(error, RunError::CommandTarget { .. }));
        assert_eq!(warms.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_warm_failure_drops_both_provider_clones_and_the_built_runtime() {
        let lifetime = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&lifetime);
        let recipe = RuntimeBuilder::default()
            .with_reusable_registered_provider(
                "tracked",
                move || {
                    let token = Arc::new(());
                    *recorded.lock().expect("lifetime recorder") = Some(Arc::downgrade(&token));
                    Ok::<_, io::Error>(TrackedProvider { token })
                },
                |_provider| async { Err::<(), _>(io::Error::other("warm refused")) },
            )
            .with_ephemeral_history()
            .into_reusable_recipe()
            .expect("recipe");

        let error = recipe
            .build()
            .await
            .expect_err("the failed warm returns no runtime");
        assert!(matches!(error, RunError::RuntimeRecipeProviderWarm(_)));
        let weak = lifetime
            .lock()
            .expect("lifetime recorder")
            .clone()
            .expect("factory ran");
        assert!(
            Weak::upgrade(&weak).is_none(),
            "the built runtime was dropped"
        );
    }

    #[derive(Clone)]
    struct TrackedProvider {
        token: Arc<()>,
    }

    #[async_trait]
    impl mentra::provider_core::ModelCatalog for TrackedProvider {
        async fn list_models(
            &self,
        ) -> Result<Vec<mentra::provider_core::ModelInfo>, ProviderError> {
            let _keep_alive = &self.token;
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl ProviderSessionFactory for TrackedProvider {
        async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
            Err(ProviderError::UnsupportedCapability(
                "test session".to_string(),
            ))
        }
    }

    #[async_trait]
    impl mentra::provider_core::Provider for TrackedProvider {
        fn definition(&self) -> ProviderDefinition {
            let mut definition = mentra::provider_core::responses::openai_definition();
            definition.descriptor = ProviderDescriptor::new("tracked");
            definition
        }
    }

    #[tokio::test]
    async fn the_private_shared_build_helper_exercises_the_recipe() {
        let runtime = recipe(RuntimeBuilder::default())
            .build()
            .await
            .expect("recipe builds");
        assert_eq!(runtime.provider(), "repeatable");
    }
}
