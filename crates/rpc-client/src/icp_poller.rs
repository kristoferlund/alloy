use alloy_json_rpc::{RpcParam, RpcReturn};
use alloy_transport::Transport;
use core::panic;
use futures::{stream, Stream};
use ic_cdk_timers::{set_timer_interval, TimerId};
use serde::Serialize;
use serde_json::value::RawValue;
use std::{borrow::Cow, cell::RefCell, marker::PhantomData, rc::Rc, time::Duration};

use crate::WeakClient;

/// A poller task builder for ICP.
///
/// This builder is used to create a poller task that repeatedly polls a method on a client and
/// invokes a callback with the responses. By default, this is done every 10 seconds, with no
/// limit on the number of successful polls. This is all configurable.
///
/// # Examples
///
/// Poll `eth_blockNumber` every 5 seconds for 10 times:
///
/// ```no_run
/// #[ic_cdk::update]
/// async fn example() -> Result<(), String> {
///
///     let config = IcpConfig::new(rpc_service);
///     let provider = ProviderBuilder::new().on_icp(config);
///
///     let callback = |incoming_blocks: Vec<FixedBytes<32>>| {
///         STATE.with_borrow_mut(|state| {
///             for block in incoming_blocks.iter() {
///                 ic_cdk::println!("{block:?}")
///             }
///         })
///     };
///
///     let poller = provider.watch_blocks().await.unwrap();
///     let timer_id = poller
///         .with_limit(Some(10))
///         .with_poll_interval(Duration::from_secs(5))
///         .start(callback)
///         .unwrap();
///
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct IcpPollerBuilder<Conn, Params, Resp> {
    client: WeakClient<Conn>,
    _pd: PhantomData<fn() -> Resp>,
    method: Cow<'static, str>,
    params: Params,
    poll_interval: Duration,
    limit: usize,
    timer_id: Option<TimerId>,
}

impl<Conn, Params, Resp> IcpPollerBuilder<Conn, Params, Resp>
where
    Conn: Transport + Clone + 'static,
    Params: RpcParam + 'static,
    Resp: RpcReturn + Clone + 'static,
{
    /// Create a new poller task.
    pub fn new(
        client: WeakClient<Conn>,
        method: impl Into<Cow<'static, str>>,
        params: Params,
    ) -> Self {
        let poll_interval =
            client.upgrade().map_or_else(|| Duration::from_secs(7), |c| c.poll_interval());
        Self {
            client,
            method: method.into(),
            params,
            timer_id: None,
            _pd: PhantomData,
            poll_interval,
            limit: usize::MAX,
        }
    }

    /// Returns the limit on the number of successful polls.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Sets a limit on the number of successful polls.
    pub fn set_limit(&mut self, limit: Option<usize>) {
        self.limit = limit.unwrap_or(usize::MAX);
    }

    /// Sets a limit on the number of successful polls.
    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.set_limit(limit);
        self
    }

    /// Returns the duration between polls.
    pub const fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Sets the duration between polls.
    pub fn set_poll_interval(&mut self, poll_interval: Duration) {
        self.poll_interval = poll_interval;
    }

    /// Sets the duration between polls.
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.set_poll_interval(poll_interval);
        self
    }

    /// Starts the poller with the given response handler.
    pub fn start<F>(mut self, response_handler: F) -> Result<TimerId, String>
    where
        F: FnMut(Resp) + 'static,
    {
        let poll_count = Rc::new(RefCell::new(0));
        let client = match WeakClient::upgrade(&self.client) {
            Some(c) => c,
            None => return Err("Client has been dropped.".into()),
        };
        let params = self.params.clone();
        let method = self.method.clone();
        let response_handler = Rc::new(RefCell::new(response_handler));

        let poll = {
            move || {
                ic_cdk::spawn({
                    let poll_count = poll_count.clone();
                    let client = client.clone();
                    let params = params.clone();
                    let method = method.clone();
                    let response_handler = response_handler.clone();

                    async move {
                        let mut params = ParamsOnce::Typed(params);
                        let params = match params.get() {
                            Ok(p) => p,
                            Err(e) => {
                                ic_cdk::println!("Failed to get params: {:?}", e);
                                return;
                            }
                        };

                        let result = client.request(method, params).await;

                        match result {
                            Ok(response) => {
                                let mut poll_count = poll_count.borrow_mut();
                                *poll_count += 1;

                                let mut handler = response_handler.borrow_mut();
                                handler(response);

                                if *poll_count >= self.limit {
                                    // Clear the timer if limit is reached
                                    if let Some(timer_id) = self.timer_id {
                                        ic_cdk_timers::clear_timer(timer_id);
                                    }
                                }
                            }
                            Err(e) => ic_cdk::println!("Request failed: {:?}", e),
                        }
                    }
                });
            }
        };

        // Initial poll
        poll();

        // Subsequent polls
        let id = set_timer_interval(self.poll_interval, poll);
        self.timer_id = Some(id);

        Ok(id)
    }

    /// Stop the poller before the limit is reached.
    pub fn stop(&mut self) {
        if let Some(timer_id) = self.timer_id.take() {
            ic_cdk_timers::clear_timer(timer_id);
        }
    }

    /// `into_stream` is not supported for ICP canisters.
    #[allow(unreachable_code)]
    pub fn into_stream(self) -> impl Stream<Item = Resp> + Unpin {
        panic!("Streams cannot be used ICP canisters.");
        stream::empty()
    }
}

// Serializes the parameters only once.
enum ParamsOnce<P> {
    Typed(P),
    Serialized(Box<RawValue>),
}

impl<P: Serialize> ParamsOnce<P> {
    #[inline]
    fn get(&mut self) -> serde_json::Result<&RawValue> {
        match self {
            Self::Typed(_) => self.init(),
            Self::Serialized(p) => Ok(p),
        }
    }

    #[cold]
    fn init(&mut self) -> serde_json::Result<&RawValue> {
        let Self::Typed(p) = self else { unreachable!() };
        let v = serde_json::value::to_raw_value(p)?;
        *self = Self::Serialized(v);
        let Self::Serialized(v) = self else { unreachable!() };
        Ok(v)
    }
}
