use crate::capture::ResponseStream;
use crate::{rocksdb::RocksDB, verify, LogHandler, Runtime};
use anyhow::Context;
use futures::{channel::mpsc, TryStreamExt};
use proto_flow::capture::{request, response, Request, Response};
use proto_flow::flow;
use proto_flow::runtime::{
    self, capture_request_ext,
    capture_response_ext::{self, PollResult},
    CaptureResponseExt,
};
use proto_gazette::consumer;
use std::pin::Pin;

pub fn run_capture<L: LogHandler>(
    delay: std::time::Duration,
    runtime: Runtime<L>,
    sessions: Vec<usize>,
    spec: &flow::CaptureSpec,
    state: models::RawValue,
    state_dir: &std::path::Path,
    timeout: std::time::Duration,
) -> impl ResponseStream {
    let spec = spec.clone();
    let state_dir = state_dir.to_owned();
    let mut state: String = state.into();

    // TODO(johnny): extract from spec?
    let version = super::unique_version();

    coroutines::try_coroutine(move |mut co| async move {
        let (mut request_tx, request_rx) = mpsc::channel(crate::CHANNEL_BUFFER);
        let response_rx = runtime.serve_capture(request_rx);
        tokio::pin!(response_rx);

        // Send Apply.
        let apply = Request {
            apply: Some(request::Apply {
                capture: Some(spec.clone()),
                dry_run: false,
                version: version.clone(),
            }),
            ..Default::default()
        };
        request_tx.try_send(Ok(apply)).expect("sender is empty");

        // Receive Applied.
        match response_rx.try_next().await? {
            Some(applied) if applied.applied.is_some() => {
                () = co.yield_(applied).await;
            }
            response => return verify("runtime", "Applied").fail(response),
        }

        let state_dir = state_dir.to_str().context("tempdir is not utf8")?;
        let rocksdb_desc = Some(runtime::RocksDbDescriptor {
            rocksdb_env_memptr: 0,
            rocksdb_path: state_dir.to_owned(),
        });
        let open_ext = capture_request_ext::Open {
            rocksdb_descriptor: rocksdb_desc.clone(),
        };

        let sessions_len = sessions.len();
        for (index, target_transactions) in sessions.into_iter().enumerate() {
            () = run_session(
                &mut co,
                delay,
                index == sessions_len - 1,
                &open_ext,
                &mut request_tx,
                &mut response_rx,
                &spec,
                &mut state,
                target_transactions,
                timeout,
                &version,
            )
            .await?;
        }

        std::mem::drop(request_tx);
        verify("runtime", "EOF").is_eof(response_rx.try_next().await?)?;

        // Re-open RocksDB.
        let rocksdb = RocksDB::open(rocksdb_desc)?;

        tracing::debug!(
            checkpoint = ?::ops::DebugJson(rocksdb.load_checkpoint()?),
            "final runtime checkpoint",
        );

        // Extract and yield the final connector state
        let state = rocksdb.load_connector_state()?;
        () = co
            .yield_(Response {
                checkpoint: Some(response::Checkpoint {
                    state: state.map(|updated_json| flow::ConnectorState {
                        updated_json,
                        merge_patch: false,
                    }),
                }),
                ..Default::default()
            })
            .await;

        anyhow::Result::Ok(())
    })
}

async fn run_session(
    co: &mut coroutines::Suspend<Response, ()>,
    delay: std::time::Duration,
    last_session: bool,
    open_ext: &capture_request_ext::Open,
    request_tx: &mut mpsc::Sender<anyhow::Result<Request>>,
    response_rx: &mut Pin<&mut impl ResponseStream>,
    spec: &flow::CaptureSpec,
    state: &mut String,
    target_transactions: usize,
    timeout: std::time::Duration,
    version: &str,
) -> anyhow::Result<()> {
    // Send Open.
    let open = Request {
        open: Some(request::Open {
            capture: Some(spec.clone()),
            version: version.to_string(),
            range: Some(flow::RangeSpec {
                key_begin: 0,
                key_end: u32::MAX,
                r_clock_begin: 0,
                r_clock_end: u32::MAX,
            }),
            state_json: std::mem::take(state),
        }),
        ..Default::default()
    }
    .with_internal(|internal| {
        internal.open = Some(open_ext.clone());
    });
    request_tx.try_send(Ok(open)).expect("sender is empty");

    // Receive Opened.
    match response_rx.try_next().await? {
        Some(opened) if opened.opened.is_some() => {
            () = co.yield_(opened).await;
        }
        response => return verify("runtime", "Opened").fail(response),
    }

    // Reset-able timer for assessing `timeout` between transactions.
    let mut deadline = tokio::time::sleep(timeout);
    let mut transaction = 0;

    while transaction != target_transactions {
        // Future which sleeps for `delay` and then sends a poll request.
        let send_poll = async {
            if !delay.is_zero() {
                () = tokio::time::sleep(delay).await;
            }
            request_tx
                .try_send(Ok(Request {
                    acknowledge: Some(request::Acknowledge { checkpoints: 0 }),
                    ..Default::default()
                }))
                .expect("sender is empty");

            Ok(())
        };

        // Join over sending a poll request and reading its result.
        let ((), poll_response) = futures::try_join!(send_poll, response_rx.try_next())?;

        let ready = {
            let verify = verify("runtime", "Poll Result");
            let poll_response = verify.not_eof(poll_response)?;
            let CaptureResponseExt {
                checkpoint:
                    Some(capture_response_ext::Checkpoint {
                        stats: None,
                        poll_result,
                    }),
                ..
            } = poll_response.get_internal()?
            else {
                return verify.fail(poll_response);
            };

            let poll_result = PollResult::from_i32(poll_result).context("invalid PollResult")?;
            tracing::debug!(?poll_result, "polled capture");

            match poll_result {
                PollResult::Invalid => return verify.fail(poll_response),
                PollResult::Ready => true,
                PollResult::CoolOff if last_session => break,
                PollResult::CoolOff | PollResult::NotReady => false,
                PollResult::Restart => break,
            }
        };

        if !ready && !timeout.is_zero() && deadline.is_elapsed() {
            break;
        } else if !ready {
            continue; // Poll again.
        }

        // Receive Captured and Checkpoint.
        let mut done = false;
        while !done {
            let verify = verify("runtime", "Captured or Checkpoint");
            let response = verify.not_eof(response_rx.try_next().await?)?;

            done = match &response {
                Response {
                    captured: Some(_), ..
                } => false,
                Response {
                    checkpoint: Some(response::Checkpoint { state }),
                    ..
                } => state.is_none(), // Final Checkpoint (only) has no `state`.
                _ => return verify.fail(response),
            };
            () = co.yield_(response).await;
        }

        // Send a StartCommit with a synthetic checkpoint that reflects our current poll.
        request_tx
            .try_send(Ok(Request::default().with_internal(|internal| {
                internal.start_commit = Some(capture_request_ext::StartCommit {
                    runtime_checkpoint: Some(consumer::Checkpoint {
                        sources: [(
                            format!("test/transactions"),
                            consumer::checkpoint::Source {
                                read_through: 1 + transaction as i64,
                                ..Default::default()
                            },
                        )]
                        .into(),
                        ack_intents: Default::default(),
                    }),
                });
            })))
            .expect("sender is empty");

        // Receive StartedCommit.
        match response_rx.try_next().await? {
            Some(Response {
                checkpoint: Some(response::Checkpoint { state: None }),
                ..
            }) => (),
            response => return verify("runtime", "StartedCommit").fail(response),
        }

        transaction += 1;

        if timeout != std::time::Duration::MAX {
            deadline = tokio::time::sleep(timeout);
        }
    }

    Ok(())
}
