// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::fmt::Debug;
use std::fmt::Formatter;
use std::io;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use async_trait::async_trait;
use bytes::Bytes;
use futures::FutureExt;
use futures::TryFutureExt;
use log::debug;
use prometheus::core::AtomicU64;
use prometheus::core::GenericCounterVec;
use prometheus::exponential_buckets;
use prometheus::histogram_opts;
use prometheus::register_histogram_vec_with_registry;
use prometheus::register_int_counter_vec_with_registry;
use prometheus::HistogramVec;
use prometheus::Registry;

use crate::ops::*;
use crate::raw::Accessor;
use crate::raw::*;
use crate::*;
/// Add [prometheus](https://docs.rs/prometheus) for every operations.
///
/// # Examples
///
/// ```
/// use log::debug;
/// use log::info;
/// use opendal::layers::PrometheusLayer;
/// use opendal::services;
/// use opendal::Operator;
/// use opendal::Result;
/// use prometheus::Encoder;
///
/// /// Visit [`opendal::services`] for more service related config.
/// /// Visit [`opendal::Object`] for more object level APIs.
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     // Pick a builder and configure it.
///     let builder = services::Memory::default();
///     let registry = prometheus::default_registry();
///
///     let op = Operator::new(builder)
///         .expect("must init")
///         .layer(PrometheusLayer::with_registry(registry.clone()))
///         .finish();
///     debug!("operator: {op:?}");
///
///     // Write data into object test.
///     op.write("test", "Hello, World!").await?;
///     // Read data from object.
///     let bs = op.read("test").await?;
///     info!("content: {}", String::from_utf8_lossy(&bs));
///
///     // Get object metadata.
///     let meta = op.stat("test").await?;
///     info!("meta: {:?}", meta);
///
///     // Export prometheus metrics.
///     let mut buffer = Vec::<u8>::new();
///     let encoder = prometheus::TextEncoder::new();
///     encoder.encode(&prometheus::gather(), &mut buffer).unwrap();
///     println!("## Prometheus Metrics");
///     println!("{}", String::from_utf8(buffer.clone()).unwrap());
///     Ok(())
/// }
/// ```
#[derive(Default, Debug, Clone)]
pub struct PrometheusLayer {
    registry: Registry,
}

impl PrometheusLayer {
    /// create PrometheusLayer by incoming registry.
    pub fn with_registry(registry: Registry) -> Self {
        Self { registry }
    }
}

impl<A: Accessor> Layer<A> for PrometheusLayer {
    type LayeredAccessor = PrometheusAccessor<A>;

    fn layer(&self, inner: A) -> Self::LayeredAccessor {
        let meta = inner.info();
        let scheme = meta.scheme();

        PrometheusAccessor {
            inner,
            stats: Arc::new(PrometheusMetrics::new(self.registry.clone())),
            scheme: scheme.to_string(),
        }
    }
}
/// [`PrometheusMetrics`] provide the performance and IO metrics.
#[derive(Debug)]
pub struct PrometheusMetrics {
    /// Total times of the specific operation be called.
    pub requests_total: GenericCounterVec<AtomicU64>,
    /// Latency of the specific operation be called.
    pub requests_duration_seconds: HistogramVec,
    /// Size of the specific metrics.
    pub bytes_total: HistogramVec,
}

impl PrometheusMetrics {
    /// new with prometheus register.
    pub fn new(registry: Registry) -> Self {
        let requests_total = register_int_counter_vec_with_registry!(
            "requests_total",
            "Total times of create be called",
            &["scheme", "operation"],
            registry
        )
        .unwrap();
        let opts = histogram_opts!(
            "requests_duration_seconds",
            "Histogram of the time spent on specific operation",
            exponential_buckets(0.01, 2.0, 16).unwrap()
        );

        let requests_duration_seconds =
            register_histogram_vec_with_registry!(opts, &["scheme", "operation"], registry)
                .unwrap();

        let opts = histogram_opts!(
            "bytes_total",
            "Total size of ",
            exponential_buckets(0.01, 2.0, 16).unwrap()
        );
        let bytes_total =
            register_histogram_vec_with_registry!(opts, &["scheme", "operation"], registry)
                .unwrap();

        Self {
            requests_total,
            requests_duration_seconds,
            bytes_total,
        }
    }

    /// error handling is the cold path, so we will not init error counters
    /// in advance.
    #[inline]
    fn increment_errors_total(&self, op: Operation, kind: ErrorKind) {
        debug!(
            "Prometheus statistics metrics error, operation {} error {}",
            op.into_static(),
            kind.into_static()
        );
    }
}

#[derive(Clone)]
pub struct PrometheusAccessor<A: Accessor> {
    inner: A,
    stats: Arc<PrometheusMetrics>,
    scheme: String,
}

impl<A: Accessor> Debug for PrometheusAccessor<A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusAccessor")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<A: Accessor> LayeredAccessor for PrometheusAccessor<A> {
    type Inner = A;
    type Reader = PrometheusMetricWrapper<A::Reader>;
    type BlockingReader = PrometheusMetricWrapper<A::BlockingReader>;
    type Writer = PrometheusMetricWrapper<A::Writer>;
    type BlockingWriter = PrometheusMetricWrapper<A::BlockingWriter>;
    type Appender = A::Appender;
    type Pager = A::Pager;
    type BlockingPager = A::BlockingPager;

    fn inner(&self) -> &Self::Inner {
        &self.inner
    }

    async fn create_dir(&self, path: &str, args: OpCreateDir) -> Result<RpCreateDir> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::CreateDir.into_static()])
            .start_timer();
        let create_res = self.inner.create_dir(path, args).await;

        timer.observe_duration();
        create_res.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::CreateDir, e.kind());
            e
        })
    }

    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::Reader)> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::Read.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::Read.into_static()])
            .start_timer();

        let read_res = self
            .inner
            .read(path, args)
            .map(|v| {
                v.map(|(rp, r)| {
                    self.stats
                        .bytes_total
                        .with_label_values(&[&self.scheme, Operation::Read.into_static()])
                        .observe(rp.metadata().content_length() as f64);
                    (
                        rp,
                        PrometheusMetricWrapper::new(
                            r,
                            Operation::Read,
                            self.stats.clone(),
                            &self.scheme,
                        ),
                    )
                })
            })
            .await;
        timer.observe_duration();
        read_res.map_err(|e| {
            self.stats.increment_errors_total(Operation::Read, e.kind());
            e
        })
    }

    async fn write(&self, path: &str, args: OpWrite) -> Result<(RpWrite, Self::Writer)> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::Write.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::Write.into_static()])
            .start_timer();

        let write_res = self
            .inner
            .write(path, args)
            .map(|v| {
                v.map(|(rp, r)| {
                    (
                        rp,
                        PrometheusMetricWrapper::new(
                            r,
                            Operation::Write,
                            self.stats.clone(),
                            &self.scheme,
                        ),
                    )
                })
            })
            .await;
        timer.observe_duration();
        write_res.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::Write, e.kind());
            e
        })
    }

    async fn append(&self, path: &str, args: OpAppend) -> Result<(RpAppend, Self::Appender)> {
        self.inner.append(path, args).await
    }

    async fn stat(&self, path: &str, args: OpStat) -> Result<RpStat> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::Stat.into_static()])
            .inc();
        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::Stat.into_static()])
            .start_timer();

        let stat_res = self
            .inner
            .stat(path, args)
            .inspect_err(|e| {
                self.stats.increment_errors_total(Operation::Stat, e.kind());
            })
            .await;
        timer.observe_duration();
        stat_res.map_err(|e| {
            self.stats.increment_errors_total(Operation::Stat, e.kind());
            e
        })
    }

    async fn delete(&self, path: &str, args: OpDelete) -> Result<RpDelete> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::Stat.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::Stat.into_static()])
            .start_timer();

        let delete_res = self.inner.delete(path, args).await;
        timer.observe_duration();
        delete_res.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::Delete, e.kind());
            e
        })
    }

    async fn list(&self, path: &str, args: OpList) -> Result<(RpList, Self::Pager)> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::List.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::List.into_static()])
            .start_timer();

        let list_res = self.inner.list(path, args).await;

        timer.observe_duration();
        list_res.map_err(|e| {
            self.stats.increment_errors_total(Operation::List, e.kind());
            e
        })
    }

    async fn batch(&self, args: OpBatch) -> Result<RpBatch> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::Batch.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::Batch.into_static()])
            .start_timer();
        let result = self.inner.batch(args).await;

        timer.observe_duration();
        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::Batch, e.kind());
            e
        })
    }

    async fn presign(&self, path: &str, args: OpPresign) -> Result<RpPresign> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::Presign.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::Presign.into_static()])
            .start_timer();
        let result = self.inner.presign(path, args).await;
        timer.observe_duration();

        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::Presign, e.kind());
            e
        })
    }

    fn blocking_create_dir(&self, path: &str, args: OpCreateDir) -> Result<RpCreateDir> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::BlockingCreateDir.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::BlockingCreateDir.into_static()])
            .start_timer();
        let result = self.inner.blocking_create_dir(path, args);

        timer.observe_duration();

        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::BlockingCreateDir, e.kind());
            e
        })
    }

    fn blocking_read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::BlockingReader)> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::BlockingRead.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme])
            .start_timer();
        let result = self.inner.blocking_read(path, args).map(|(rp, r)| {
            self.stats
                .bytes_total
                .with_label_values(&[&self.scheme, Operation::BlockingRead.into_static()])
                .observe(rp.metadata().content_length() as f64);
            (
                rp,
                PrometheusMetricWrapper::new(
                    r,
                    Operation::BlockingRead,
                    self.stats.clone(),
                    &self.scheme,
                ),
            )
        });
        timer.observe_duration();
        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::BlockingRead, e.kind());
            e
        })
    }

    fn blocking_write(&self, path: &str, args: OpWrite) -> Result<(RpWrite, Self::BlockingWriter)> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::BlockingWrite.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::BlockingWrite.into_static()])
            .start_timer();
        let result = self.inner.blocking_write(path, args).map(|(rp, r)| {
            (
                rp,
                PrometheusMetricWrapper::new(
                    r,
                    Operation::BlockingWrite,
                    self.stats.clone(),
                    &self.scheme,
                ),
            )
        });
        timer.observe_duration();
        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::BlockingWrite, e.kind());
            e
        })
    }

    fn blocking_stat(&self, path: &str, args: OpStat) -> Result<RpStat> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::BlockingStat.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::BlockingStat.into_static()])
            .start_timer();
        let result = self.inner.blocking_stat(path, args);
        timer.observe_duration();
        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::BlockingStat, e.kind());
            e
        })
    }

    fn blocking_delete(&self, path: &str, args: OpDelete) -> Result<RpDelete> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::BlockingDelete.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::BlockingDelete.into_static()])
            .start_timer();
        let result = self.inner.blocking_delete(path, args);
        timer.observe_duration();

        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::BlockingDelete, e.kind());
            e
        })
    }

    fn blocking_list(&self, path: &str, args: OpList) -> Result<(RpList, Self::BlockingPager)> {
        self.stats
            .requests_total
            .with_label_values(&[&self.scheme, Operation::BlockingList.into_static()])
            .inc();

        let timer = self
            .stats
            .requests_duration_seconds
            .with_label_values(&[&self.scheme, Operation::BlockingList.into_static()])
            .start_timer();
        let result = self.inner.blocking_list(path, args);
        timer.observe_duration();

        result.map_err(|e| {
            self.stats
                .increment_errors_total(Operation::BlockingList, e.kind());
            e
        })
    }
}

pub struct PrometheusMetricWrapper<R> {
    inner: R,

    op: Operation,
    stats: Arc<PrometheusMetrics>,
    scheme: String,
}

impl<R> PrometheusMetricWrapper<R> {
    fn new(inner: R, op: Operation, stats: Arc<PrometheusMetrics>, scheme: &String) -> Self {
        Self {
            inner,
            op,
            stats,
            scheme: scheme.to_string(),
        }
    }
}

impl<R: oio::Read> oio::Read for PrometheusMetricWrapper<R> {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize>> {
        self.inner.poll_read(cx, buf).map(|res| match res {
            Ok(bytes) => {
                self.stats
                    .bytes_total
                    .with_label_values(&[&self.scheme, Operation::Read.into_static()])
                    .observe(bytes as f64);
                Ok(bytes)
            }
            Err(e) => {
                self.stats.increment_errors_total(self.op, e.kind());
                Err(e)
            }
        })
    }

    fn poll_seek(&mut self, cx: &mut Context<'_>, pos: io::SeekFrom) -> Poll<Result<u64>> {
        self.inner.poll_seek(cx, pos).map(|res| match res {
            Ok(n) => Ok(n),
            Err(e) => {
                self.stats.increment_errors_total(self.op, e.kind());
                Err(e)
            }
        })
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes>>> {
        self.inner.poll_next(cx).map(|res| match res {
            Some(Ok(bytes)) => {
                self.stats
                    .bytes_total
                    .with_label_values(&[&self.scheme, Operation::Read.into_static()])
                    .observe(bytes.len() as f64);
                Some(Ok(bytes))
            }
            Some(Err(e)) => {
                self.stats.increment_errors_total(self.op, e.kind());
                Some(Err(e))
            }
            None => None,
        })
    }
}

impl<R: oio::BlockingRead> oio::BlockingRead for PrometheusMetricWrapper<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.inner
            .read(buf)
            .map(|n| {
                self.stats
                    .bytes_total
                    .with_label_values(&[&self.scheme, Operation::BlockingRead.into_static()])
                    .observe(n as f64);
                n
            })
            .map_err(|e| {
                self.stats.increment_errors_total(self.op, e.kind());
                e
            })
    }

    fn seek(&mut self, pos: io::SeekFrom) -> Result<u64> {
        self.inner.seek(pos).map_err(|err| {
            self.stats.increment_errors_total(self.op, err.kind());
            err
        })
    }

    fn next(&mut self) -> Option<Result<Bytes>> {
        self.inner.next().map(|res| match res {
            Ok(bytes) => {
                self.stats
                    .bytes_total
                    .with_label_values(&[&self.scheme, Operation::BlockingRead.into_static()])
                    .observe(bytes.len() as f64);
                Ok(bytes)
            }
            Err(e) => {
                self.stats.increment_errors_total(self.op, e.kind());
                Err(e)
            }
        })
    }
}

#[async_trait]
impl<R: oio::Write> oio::Write for PrometheusMetricWrapper<R> {
    async fn write(&mut self, bs: Bytes) -> Result<()> {
        let size = bs.len();
        self.inner
            .write(bs)
            .await
            .map(|_| {
                self.stats
                    .bytes_total
                    .with_label_values(&[&self.scheme, Operation::Write.into_static()])
                    .observe(size as f64)
            })
            .map_err(|err| {
                self.stats.increment_errors_total(self.op, err.kind());
                err
            })
    }

    async fn abort(&mut self) -> Result<()> {
        self.inner.abort().await.map_err(|err| {
            self.stats.increment_errors_total(self.op, err.kind());
            err
        })
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.close().await.map_err(|err| {
            self.stats.increment_errors_total(self.op, err.kind());
            err
        })
    }
}

impl<R: oio::BlockingWrite> oio::BlockingWrite for PrometheusMetricWrapper<R> {
    fn write(&mut self, bs: Bytes) -> Result<()> {
        let size = bs.len();
        self.inner
            .write(bs)
            .map(|_| {
                self.stats
                    .bytes_total
                    .with_label_values(&[&self.scheme, Operation::BlockingWrite.into_static()])
                    .observe(size as f64)
            })
            .map_err(|err| {
                self.stats.increment_errors_total(self.op, err.kind());
                err
            })
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close().map_err(|err| {
            self.stats.increment_errors_total(self.op, err.kind());
            err
        })
    }
}
