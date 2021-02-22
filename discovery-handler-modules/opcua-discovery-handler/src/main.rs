use akri_discovery_utils::{
    discovery::{server::run_discovery_server, DISCOVERY_HANDLER_PATH},
    registration_client::{register, register_again},
};
use akri_opcua::{
    discovery_handler::{DiscoveryHandler, DISCOVERY_PORT},
    get_register_request,
};
use log::{info, trace};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    env_logger::try_init()?;
    info!("main - opcua discovery handler started");
    // Determine whether to serve discovery handler over UDS or IP based on existence
    // of the environment variable POD_IP.
    let mut use_uds = true;
    let mut endpoint: String = match std::env::var("POD_IP") {
        Ok(pod_ip) => {
            trace!("main - registering with Agent with IP endpoint");
            use_uds = false;
            format!("{}:{}", pod_ip, DISCOVERY_PORT)
        }
        Err(_) => {
            trace!("main - registering with Agent with uds endpoint");
            format!("{}/opcua.sock", DISCOVERY_HANDLER_PATH)
        }
    };
    let (register_sender, register_receiver) = tokio::sync::mpsc::channel(2);
    let endpoint_clone = endpoint.clone();
    let discovery_handle = tokio::spawn(async move {
        run_discovery_server(
            DiscoveryHandler::new(Some(register_sender)),
            &endpoint_clone,
        )
        .await
        .unwrap();
    });
    if !use_uds {
        endpoint.insert_str(0, "http://");
    }
    let register_request = get_register_request(&endpoint);
    register(&register_request).await?;
    let registration_handle = tokio::spawn(async move {
        register_again(register_receiver, &register_request).await;
    });
    tokio::try_join!(discovery_handle, registration_handle)?;
    info!("main - opcua discovery handler ended");
    Ok(())
}
