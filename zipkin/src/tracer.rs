//  Copyright 2017 Palantir Technologies, Inc.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

//! Tracers.
use rand::{self, Rng};
use std::marker::PhantomData;
use std::mem;
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use thread_local_object::ThreadLocal;

use {Annotation, Endpoint, Kind, Report, Sample, SamplingFlags, Span, SpanId, TraceContext,
     TraceId};
use report::LoggingReporter;
use sample::AlwaysSampler;
use span;

enum SpanState {
    Real {
        span: span::Builder,
        start_instant: Instant,
    },
    Nop,
}

/// A guard object for the thread-local current trace context.
///
/// It will restore the previous trace context when it drops.
pub struct CurrentGuard {
    tracer: Tracer,
    prev: Option<TraceContext>,
    // make sure this type is !Send and !Sync since it pokes at thread locals
    _p: PhantomData<*const ()>,
}

impl Drop for CurrentGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => {
                self.tracer.0.current.set(prev);
            }
            None => {
                self.tracer.0.current.remove();
            }
        }
    }
}

/// An open span.
///
/// This is a guard object - the span will be finished and reported when it
/// falls out of scope.
pub struct OpenSpan {
    guard: CurrentGuard,
    context: TraceContext,
    state: SpanState,
}

impl Drop for OpenSpan {
    fn drop(&mut self) {
        if let SpanState::Real {
            mut span,
            start_instant,
        } = mem::replace(&mut self.state, SpanState::Nop)
        {
            span.duration(start_instant.elapsed());
            if let Some(parent_id) = self.context.parent_id() {
                span.parent_id(parent_id);
            }
            let span = span.build(self.context.trace_id(), self.context.span_id());

            self.guard.tracer.0.reporter.report(&span);
        }
    }
}

impl OpenSpan {
    /// Returns the context associated with this span.
    pub fn context(&self) -> TraceContext {
        self.context
    }

    /// Sets the name of this span.
    pub fn name(&mut self, name: &str) {
        if let SpanState::Real { ref mut span, .. } = self.state {
            span.name(name);
        }
    }

    /// Sets the kind of this span.
    pub fn kind(&mut self, kind: Kind) {
        if let SpanState::Real { ref mut span, .. } = self.state {
            span.kind(kind);
        }
    }

    /// Sets the remote endpoint of this span.
    pub fn remote_endpoint(&mut self, remote_endpoint: Endpoint) {
        if let SpanState::Real { ref mut span, .. } = self.state {
            span.remote_endpoint(remote_endpoint);
        }
    }

    /// Attaches an annotation to this span.
    pub fn annotate(&mut self, value: &str) {
        if let SpanState::Real { ref mut span, .. } = self.state {
            let annotation = Annotation::now(value);
            span.annotation(annotation);
        }
    }

    /// Attaches a tag to this span.
    pub fn tag(&mut self, key: &str, value: &str) {
        if let SpanState::Real { ref mut span, .. } = self.state {
            span.tag(key, value);
        }
    }
}

struct Inner {
    current: ThreadLocal<TraceContext>,
    local_endpoint: Endpoint,
    reporter: Box<Report + Sync + Send>,
    sampler: Box<Sample + Sync + Send>,
}

/// The root tracing object.
///
/// Each thread has its own current span state - the `Tracer` should be a single
/// global resource.
#[derive(Clone)]
pub struct Tracer(Arc<Inner>);

impl Tracer {
    /// Creates a `Tracer` builder.
    pub fn builder() -> Builder {
        Builder {
            reporter: None,
            sampler: None,
        }
    }

    /// Starts a new trace with no parent.
    pub fn new_trace(&self) -> OpenSpan {
        self.new_trace_from(SamplingFlags::default())
    }

    /// Starts a new trace with no parent with specific sampling flags.
    pub fn new_trace_from(&self, flags: SamplingFlags) -> OpenSpan {
        let id = self.next_id();
        let context = TraceContext::builder()
            .sampling_flags(flags)
            .build(TraceId::from(id), SpanId::from(id));
        self.ensure_sampled(context, false)
    }

    /// Joins an existing trace.
    ///
    /// The context can come from, for example, the headers of an HTTP request.
    pub fn join_trace(&self, context: TraceContext) -> OpenSpan {
        self.ensure_sampled(context, true)
    }

    /// Starts a new span with the specified parent.
    pub fn new_child(&self, parent: TraceContext) -> OpenSpan {
        let id = self.next_id();
        let context = TraceContext::builder()
            .parent_id(parent.span_id())
            .sampling_flags(parent.sampling_flags())
            .build(parent.trace_id(), SpanId::from(id));
        self.ensure_sampled(context, false)
    }

    /// Starts a new trace parented to the current span if one exists.
    pub fn next_span(&self) -> OpenSpan {
        match self.current() {
            Some(context) => self.new_child(context),
            None => self.new_trace(),
        }
    }

    fn next_id(&self) -> [u8; 8] {
        let mut id = [0; 8];
        rand::thread_rng().fill_bytes(&mut id);
        id
    }

    fn ensure_sampled(&self, mut context: TraceContext, mut shared: bool) -> OpenSpan {
        if let None = context.sampled() {
            context.flags.sampled = Some(self.0.sampler.sample(context.trace_id()));
            // since the thing we got this context from didn't indicate if it should be sampled
            // we can't assume they're recording the span as well.
            shared = false;
        }

        let state = match context.sampled() {
            Some(false) => SpanState::Nop,
            _ => {
                let mut span = Span::builder();
                span.timestamp(SystemTime::now())
                    .shared(shared)
                    .local_endpoint(self.0.local_endpoint.clone());

                SpanState::Real {
                    span,
                    start_instant: Instant::now(),
                }
            }
        };

        self.new_span(context, state)
    }

    fn new_span(&self, context: TraceContext, state: SpanState) -> OpenSpan {
        let guard = self.set_current(context);

        OpenSpan {
            guard,
            context,
            state,
        }
    }

    /// Sets this thread's current trace context.
    ///
    /// This method does not start a span. It is designed to be used when
    /// propagating the trace of an existing span to a new thread.
    ///
    /// A guard object is returned which will restore the previous trace context
    /// when it falls out of scope.
    pub fn set_current(&self, context: TraceContext) -> CurrentGuard {
        CurrentGuard {
            tracer: self.clone(),
            prev: self.0.current.set(context),
            _p: PhantomData,
        }
    }

    /// Returns this thread's current trace context.
    pub fn current(&self) -> Option<TraceContext> {
        self.0.current.get_cloned()
    }
}

/// A builder type for `Tracer`s.
pub struct Builder {
    reporter: Option<Box<Report + Sync + Send>>,
    sampler: Option<Box<Sample + Sync + Send>>,
}

impl Builder {
    /// Sets the reporter which consumes completed spans.
    ///
    /// Defaults to the `LoggingReporter`.
    pub fn reporter(&mut self, reporter: Box<Report + Sync + Send>) -> &mut Builder {
        self.reporter = Some(reporter);
        self
    }

    /// Sets the sampler which determines if a trace should be tracked and reported.
    ///
    /// Defaults to the `AlwaysSampler`.
    pub fn sampler(&mut self, sampler: Box<Sample + Sync + Send>) -> &mut Builder {
        self.sampler = Some(sampler);
        self
    }

    /// Constructs a new `Tracer`.
    pub fn build(&mut self, local_endpoint: Endpoint) -> Tracer {
        let reporter = self.reporter
            .take()
            .unwrap_or_else(|| Box::new(LoggingReporter));

        let sampler = self.sampler
            .take()
            .unwrap_or_else(|| Box::new(AlwaysSampler));

        let inner = Inner {
            current: ThreadLocal::new(),
            local_endpoint,
            reporter,
            sampler,
        };

        Tracer(Arc::new(inner))
    }
}
