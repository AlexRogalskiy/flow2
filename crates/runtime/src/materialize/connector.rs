use crate::{unseal, LogHandler, Runtime};
use anyhow::Context;
use futures::{channel::mpsc, stream::BoxStream, FutureExt, StreamExt};
use proto_flow::{
    flow::materialization_spec::ConnectorType,
    materialize::{Request, Response},
    runtime::MaterializeRequestExt,
};

// Start a materialization connector as indicated by the `initial` Request.
// Returns a pair of Streams for sending Requests and receiving Responses.
pub async fn start<L: LogHandler>(
    runtime: &Runtime<L>,
    mut initial: Request,
) -> anyhow::Result<(
    mpsc::Sender<Request>,
    BoxStream<'static, anyhow::Result<Response>>,
)> {
    let (endpoint, log_level, config_json) = extract_endpoint(&mut initial)?;
    let (mut connector_tx, connector_rx) = mpsc::channel(crate::CHANNEL_BUFFER);

    // Adjust the dynamic log level for this connector's lifecycle.
    if let (Some(log_level), Some(set_log_level)) = (log_level, &runtime.set_log_level) {
        (set_log_level)(log_level);
    }

    fn attach_container(response: &mut Response, container: crate::image_connector::Container) {
        response.set_internal(|internal| {
            internal.container = Some(container);
        });
    }

    fn start_rpc(
        channel: tonic::transport::Channel,
        rx: mpsc::Receiver<Request>,
    ) -> crate::image_connector::StartRpcFuture<Response> {
        async move {
            proto_grpc::materialize::connector_client::ConnectorClient::new(channel)
                .materialize(rx)
                .await
        }
        .boxed()
    }

    let connector_rx = match endpoint {
        models::MaterializationEndpoint::Connector(models::ConnectorConfig {
            image,
            config: sealed_config,
        }) => {
            *config_json = unseal::decrypt_sops(&sealed_config).await?.to_string();
            connector_tx.try_send(initial).unwrap();

            crate::image_connector::serve(
                attach_container,
                image,
                runtime.log_handler.clone(),
                log_level,
                &runtime.container_network,
                connector_rx,
                start_rpc,
                &runtime.task_name,
                ops::TaskType::Derivation,
            )
            .await?
            .boxed()
        }
        models::MaterializationEndpoint::Local(_) if !runtime.allow_local => {
            return Err(tonic::Status::failed_precondition(
                "Local connectors are not permitted in this context",
            )
            .into());
        }
        models::MaterializationEndpoint::Local(models::LocalConfig {
            command,
            config: sealed_config,
            env,
            protobuf,
        }) => {
            *config_json = unseal::decrypt_sops(&sealed_config).await?.to_string();
            connector_tx.try_send(initial).unwrap();

            crate::local_connector::serve(
                command,
                env,
                runtime.log_handler.clone(),
                log_level,
                protobuf,
                connector_rx,
            )?
            .boxed()
        }
    };

    Ok((connector_tx, connector_rx))
}

fn extract_endpoint<'r>(
    request: &'r mut Request,
) -> anyhow::Result<(
    models::MaterializationEndpoint,
    Option<ops::LogLevel>,
    &'r mut String,
)> {
    let ext_log_level = match request.get_internal() {
        Ok(MaterializeRequestExt {
            labels: Some(labels),
            ..
        }) => Some(labels.log_level()),
        _ => None,
    };

    let (connector_type, log_level, config_json) = match request {
        Request {
            spec: Some(spec), ..
        } => (spec.connector_type, ext_log_level, &mut spec.config_json),
        Request {
            validate: Some(validate),
            ..
        } => (
            validate.connector_type,
            ext_log_level,
            &mut validate.config_json,
        ),
        Request {
            apply: Some(apply), ..
        } => {
            let inner = apply
                .materialization
                .as_mut()
                .context("`apply` missing required `materialization`")?;

            (
                inner.connector_type,
                crate::shard_log_level(inner.shard_template.as_ref()),
                &mut inner.config_json,
            )
        }
        Request {
            open: Some(open), ..
        } => {
            let inner = open
                .materialization
                .as_mut()
                .context("`open` missing required `materialization`")?;

            (
                inner.connector_type,
                crate::shard_log_level(inner.shard_template.as_ref()),
                &mut inner.config_json,
            )
        }
        request => return crate::verify("client", "valid first request").fail(request),
    };

    if connector_type == ConnectorType::Image as i32 {
        Ok((
            models::MaterializationEndpoint::Connector(
                serde_json::from_str(config_json).context("parsing connector config")?,
            ),
            log_level,
            config_json,
        ))
    } else if connector_type == ConnectorType::Local as i32 {
        Ok((
            models::MaterializationEndpoint::Local(
                serde_json::from_str(config_json).context("parsing local config")?,
            ),
            log_level,
            config_json,
        ))
    } else {
        anyhow::bail!("invalid connector type: {connector_type}");
    }
}
