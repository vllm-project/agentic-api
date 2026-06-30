mod accumulator_throughput;
mod executor_throughput;
mod storage_concurrent;
mod storage_crud;

use criterion::criterion_main;

criterion_main!(
    storage_crud::storage_benches,
    storage_concurrent::storage_concurrent_benches,
    executor_throughput::executor_benches,
    accumulator_throughput::accumulator_benches,
);
