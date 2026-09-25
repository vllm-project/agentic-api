use super::*;
use futures::StreamExt;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn event(sequence_number: u64) -> StreamEvent {
    StreamEvent {
        content: "event".into(),
        sequence_number,
    }
}

#[tokio::test]
async fn dropping_unpolled_stream_destroys_producer_captures_without_starting_it() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(Arc::clone(&dropped));
    let (sender, receiver) = mpsc::channel(1);
    let producer = async move {
        let _guard = guard;
        panic!("must not start an unpolled producer");
    };
    drop(drive::<()>(producer, receiver));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(sender.is_closed());
}

#[tokio::test]
async fn backpressured_producer_stops_polling_and_is_dropped_synchronously() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let sent = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(Arc::clone(&dropped));
    let observed = Arc::clone(&sent);
    let (sender, receiver) = mpsc::channel(1);
    let producer = async move {
        let _guard = guard;
        for sequence in 0..10 {
            sender.send(event(sequence)).await.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
        }
    };
    let mut stream = Box::pin(drive(producer, receiver));
    assert!(matches!(stream.next().await, Some(ProducerEvent::Event(_))));
    let at_yield = sent.load(Ordering::SeqCst);
    assert!(
        (1..=2).contains(&at_yield),
        "one delivered event and at most one queued event"
    );
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert_eq!(sent.load(Ordering::SeqCst), at_yield, "no background producer progress");
    drop(stream);
    // Deliberately no await/yield between dropping the stream and this assertion.
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_io_is_cancelled_when_the_consumer_future_is_dropped() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(Arc::clone(&dropped));
    let (_sender, receiver) = mpsc::channel(1);
    let producer = async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    };
    let mut stream = Box::pin(drive(producer, receiver));
    assert!(futures::poll!(stream.next()).is_pending());
    drop(stream);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn queued_events_precede_one_completion_and_a_closed_channel_does_not_spin() {
    let (sender, receiver) = mpsc::channel(3);
    let producer = async move {
        for sequence in 0..3 {
            sender.send(event(sequence)).await.unwrap();
        }
        42
    };
    let mut stream = Box::pin(drive(producer, receiver));
    for expected in 0..3 {
        let Some(ProducerEvent::Event(event)) = stream.next().await else {
            panic!("queued event")
        };
        assert_eq!(event.sequence_number, expected);
    }
    assert!(matches!(stream.next().await, Some(ProducerEvent::Finished(Ok(42)))));
    assert!(stream.next().await.is_none());

    let (sender, receiver) = mpsc::channel(1);
    drop(sender);
    let mut stream = Box::pin(drive(async { 7 }, receiver));
    assert!(matches!(stream.next().await, Some(ProducerEvent::Finished(Ok(7)))));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn panic_disposes_producer_before_delivering_error_and_preserves_queued_order() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(Arc::clone(&dropped));
    let (sender, receiver) = mpsc::channel(2);
    let producer = async move {
        let _guard = guard;
        sender.send(event(0)).await.unwrap();
        sender.send(event(1)).await.unwrap();
        panic!("private panic payload");
    };
    let mut stream = Box::pin(drive::<()>(producer, receiver));
    for expected in 0..2 {
        let Some(ProducerEvent::Event(event)) = stream.next().await else {
            panic!("queued event")
        };
        assert_eq!(event.sequence_number, expected);
    }
    let Some(ProducerEvent::Finished(Err(error))) = stream.next().await else {
        panic!("panic outcome")
    };
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(matches!(error, ExecutorError::StreamProducerPanicked));
    assert!(!format!("{error:?} {error}").contains("private panic payload"));
    let chunk = crate::executor::gateway_accumulator::GatewayStreamAccumulator::executor_error_chunk_at(&error, 2);
    assert!(chunk.contains("\"sequence_number\":2"));
    assert!(!chunk.contains("private panic payload"));
    assert!(stream.next().await.is_none());
}
