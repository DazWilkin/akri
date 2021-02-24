use akri_discovery_utils::discovery::discovery_handler::run_discovery_handler;
use akri_onvif::discovery_handler::DiscoveryHandler;
use log::info;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    env_logger::try_init()?;
    info!("main - onvif discovery handler started");
    let (register_sender, register_receiver) = tokio::sync::mpsc::channel(2);
    let discovery_handler = DiscoveryHandler::new(Some(register_sender));
    run_discovery_handler(discovery_handler, register_receiver, "onvif", false).await?;
    info!("main - onvif discovery handler ended");
    Ok(())
}
