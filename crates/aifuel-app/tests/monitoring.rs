use aifuel_app::MonitoringFacade;
use aifuel_core::{StatusCollector, StatusReport};
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ControlledCollector {
    calls: Arc<AtomicUsize>,
}

impl StatusCollector for ControlledCollector {
    fn collect_status(&self) -> Pin<Box<dyn Future<Output = StatusReport> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { StatusReport::cold(1.0) })
    }
}

#[tokio::test]
async fn cache_only_status_reads_reuse_the_application_collection() {
    let collector = ControlledCollector {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let calls = Arc::clone(&collector.calls);
    let facade = MonitoringFacade::new(collector);

    let first = facade.status(false).await;
    let second = facade.status(false).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.generated_at, 1.0);
    assert_eq!(second.generated_at, 1.0);
}

#[tokio::test]
async fn cache_only_read_never_starts_a_collection() {
    let calls = Arc::new(AtomicUsize::new(0));
    let facade = MonitoringFacade::new(ControlledCollector {
        calls: Arc::clone(&calls),
    });

    assert!(facade.cached_status().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    facade.collect().await;

    assert_eq!(
        facade.cached_status().map(|report| report.generated_at),
        Some(1.0)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
