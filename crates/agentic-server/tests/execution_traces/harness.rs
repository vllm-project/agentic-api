use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn trace_test() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().await;
    EXPORTER
        .get_or_init(|| {
            opentelemetry::global::set_text_map_propagator(
                opentelemetry_sdk::propagation::TraceContextPropagator::new(),
            );
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            tracing::subscriber::set_global_default(agentic_server::telemetry::build_subscriber(
                Some(provider.tracer("test")),
                tracing_subscriber::EnvFilter::new("off"),
                std::io::sink,
            ))
            .unwrap();
            std::mem::forget(provider);
            exporter
        })
        .reset();
    guard
}

pub fn spans() -> Vec<SpanData> {
    EXPORTER.get().unwrap().get_finished_spans().unwrap()
}

pub async fn finished(name: &str, count: usize) -> Vec<SpanData> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let spans = spans();
            if spans.iter().filter(|span| span.name == name).count() >= count {
                super::trace_attributes::assert_allowed(&spans, &[]);
                return spans;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {count} {name} spans: {:?}", spans()))
}

pub fn attr<'a>(span: &'a SpanData, name: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == name)
        .map(|kv| &kv.value)
}

pub struct Server {
    pub url: String,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    pub async fn start(router: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, task: Some(task) }
    }

    pub async fn stop(mut self) {
        let task = self.task.take().unwrap();
        task.abort();
        let _ = task.await;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub async fn gateway(upstream: &str) -> Server {
    let state = super::common::test_state(&super::common::test_config(upstream));
    Server::start(agentic_server::app::build_router(
        state,
        &agentic_server::app::ServerConfig::from_env(),
    ))
    .await
}
