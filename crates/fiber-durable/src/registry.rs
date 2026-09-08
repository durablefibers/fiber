use crate::context::FiberContext;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[async_trait]
pub trait FiberHandler: Send + Sync {
    async fn run(&self, ctx: &mut FiberContext) -> Result<Value>;
}

type BoxedHandler = Arc<dyn FiberHandler>;

#[derive(Clone, Default)]
pub struct FiberRegistry {
    handlers: Arc<std::sync::RwLock<HashMap<String, BoxedHandler>>>,
}

impl FiberRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<H: FiberHandler + 'static>(&self, name: impl Into<String>, handler: H) {
        self.handlers
            .write()
            .expect("registry lock")
            .insert(name.into(), Arc::new(handler));
    }

    pub fn register_fn<F, Fut>(&self, name: impl Into<String>, f: F)
    where
        F: Fn(&mut FiberContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value>> + Send,
    {
        struct FnHandler<F>(F);
        #[async_trait]
        impl<F, Fut> FiberHandler for FnHandler<F>
        where
            F: Fn(&mut FiberContext) -> Fut + Send + Sync,
            Fut: std::future::Future<Output = Result<Value>> + Send,
        {
            async fn run(&self, ctx: &mut FiberContext) -> Result<Value> {
                (self.0)(ctx).await
            }
        }
        self.register(name, FnHandler(f));
    }

    pub fn get(&self, name: &str) -> Option<BoxedHandler> {
        self.handlers
            .read()
            .expect("registry lock")
            .get(name)
            .cloned()
    }

    /// Registered task names, sorted. What `GET /api/fibers/tasks` serves, so the UI offers
    /// what this build actually has rather than a list copied into the frontend.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .handlers
            .read()
            .expect("registry lock")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers
            .read()
            .expect("registry lock")
            .contains_key(name)
    }
}
