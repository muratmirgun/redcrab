use std::sync::OnceLock;

use anyhow::Result;
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Two I/O threads are plenty: every connection is multiplexed, and the work
/// per response is parsing, not computation.
pub(crate) fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("redcrab-io")
            .enable_all()
            .build()
            .expect("failed to start the redcrab I/O runtime")
    })
}

/// Runs `fut` on the I/O runtime. The returned future can be awaited from any
/// executor.
pub(crate) async fn spawn<T, F>(fut: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    runtime().spawn(fut).await?
}
