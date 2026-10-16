use anyhow::{Context as _, Result};
use home_maintenance::{api, config::Config, store::Store, supervisor::Supervisor};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    if let Some(path) = &config.openapi {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&api::openapi()?)?),
        )?;
        return Ok(());
    }
    let logger = home_maintenance::logging::logger();
    let store = Store::open(&config.database).await?;
    let mut context = api::Context::new(
        store.clone(),
        config
            .api_token
            .as_deref()
            .context("owner credential required")?,
    );
    if let Some(token) = &config.integration_token {
        context = context.with_integration(token);
    }
    let server = api::start_with_logger(config.bind, Arc::new(context.clone()), logger.clone())?;
    let ingress = config
        .ingress_bind
        .map(|bind| {
            api::start_with_logger(
                bind,
                Arc::new(context.for_ingress(config.ingress_peer)),
                logger.clone(),
            )
        })
        .transpose()?;
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let discovery = if let (Some(supervisor), Some(token)) =
        (Supervisor::from_environment()?, config.integration_token)
    {
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {_=stopped.changed()=>break,result=supervisor.announce(&token)=>{if result.is_err(){slog::warn!(logger,"integration_discovery_failed");}}}
                tokio::select! {_=stopped.changed()=>break,_=tokio::time::sleep(std::time::Duration::from_secs(60))=>{}}
            }
        }))
    } else {
        None
    };
    shutdown().await?;
    // Detached mutations finish before discovery and SQLite are shut down.
    let api_result = server.close().await;
    let ingress_result = if let Some(ingress) = ingress {
        ingress.close().await
    } else {
        Ok(())
    };
    let _ = stop.send(true);
    if let Some(discovery) = discovery {
        discovery.await?;
    }
    let database_result = store.close().await;
    api_result.map_err(|_| anyhow::anyhow!("close API listener"))?;
    ingress_result.map_err(|_| anyhow::anyhow!("close ingress listener"))?;
    database_result?;
    Ok(())
}
async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
