
use akri_discovery_utils::discovery::v0::{Device, DiscoverResponse, DiscoverRequest, discovery_server::{Discovery, DiscoveryServer}};
use anyhow::Error;
use async_trait::async_trait;
use std::{collections::HashMap, fs};
use tokio::sync::mpsc;
use tokio::time::delay_for;
use log::{error, info, trace};
use std::time::Duration;
use tonic::{transport::Server, Response, Status};

/// Protocol name that debugEcho discovery handlers use when registering with the Agent
pub const PROTOCOL_NAME: &str = "debugEcho";
/// Endpoint for the debugEcho discovery services
pub const DISCOVERY_ENDPOINT: &str = "[::1]:10001";
// TODO: make this configurable
pub const DISCOVERY_INTERVAL_SECS: u64 = 10;

/// File acting as an environment variable for testing discovery.
/// To mimic an instance going offline, kubectl exec into one of the akri-agent-daemonset pods
/// and echo "OFFLINE" > /tmp/debug-echo-availability.txt
/// To mimic a device coming back online, remove the word "OFFLINE" from the file
/// ie: echo "" > /tmp/debug-echo-availability.txt
pub const DEBUG_ECHO_AVAILABILITY_CHECK_PATH: &str = "/tmp/debug-echo-availability.txt";
/// String to write into DEBUG_ECHO_AVAILABILITY_CHECK_PATH to make Other devices undiscoverable
pub const OFFLINE: &str = "OFFLINE";
pub type DiscoverStream = mpsc::Receiver<Result<DiscoverResponse, Status>>;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub enum DiscoveryHandlerType {
    DebugEcho(DebugEchoDiscoveryHandlerConfig),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DebugEchoDiscoveryHandlerConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub descriptions: Vec<String>,
}

/// This defines the DebugEcho data stored in the Configuration
/// CRD
///
/// DebugEcho is used for testing Akri.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DebugEchoDiscoveryHandler {
}

/// `DebugEchoDiscoveryHandler` contains a `DebugEchoDiscoveryHandlerConfig` which has a
/// list of mock instances (`discovery_handler_config.descriptions`) and their sharability.
/// It mocks discovering the instances by inspecting the contents of the file at `DEBUG_ECHO_AVAILABILITY_CHECK_PATH`.
/// If the file contains "OFFLINE", it won't discover any of the instances, else it discovers them all.
impl DebugEchoDiscoveryHandler {
    pub fn new() -> Self {
        DebugEchoDiscoveryHandler {
        }
    }
}

#[async_trait]
impl Discovery for DebugEchoDiscoveryHandler {
    type DiscoverStream = DiscoverStream;
    async fn discover(& self, request: tonic::Request<DiscoverRequest>) -> Result<Response<Self::DiscoverStream>, Status> {
        info!("discover - called for debug echo protocol");
        let discover_request = request.get_ref();
        let (mut tx, rx) = mpsc::channel(4);
        let discovery_handler_config = get_configuration(&discover_request.discovery_details).map_err(|e| {
            tonic::Status::new(
                tonic::Code::InvalidArgument,
                format!("Invalid debugEcho discovery handler configuration: {}", e),
            )
        })?;
        let descriptions = discovery_handler_config.descriptions;
        let mut availability =
                    fs::read_to_string(DEBUG_ECHO_AVAILABILITY_CHECK_PATH).unwrap_or_default();
        let mut offline =  availability.contains(OFFLINE);
        let mut first_loop = true;
        tokio::spawn(async move {
            loop {
                availability = fs::read_to_string(DEBUG_ECHO_AVAILABILITY_CHECK_PATH).unwrap_or_default();
                info!(
                    "discover -- debugEcho devices are online? {}",
                    !availability.contains(OFFLINE)
                );
                if (availability.contains(OFFLINE) && !offline) || offline && first_loop {
                    if first_loop {
                        first_loop = false;
                    }
                    // If the device is now offline, return an empty list of instance info
                    offline = true;
                    if let Err(e) = tx.send(Ok(DiscoverResponse{ devices: Vec::new()})).await {
                        error!("discover - for debugEcho failed to send discovery response with error {}", e);
                        break;
                    }
                } else if (!availability.contains(OFFLINE) && offline) || !offline && first_loop {
                    if first_loop {
                        first_loop = false;
                    }
                    offline = false;
                    let devices = descriptions
                    .iter()
                    .map(|description| {
                        Device
                        {
                            id: description.clone(),
                            properties: HashMap::new(),
                            mounts: Vec::default(),
                            device_specs: Vec::default(),

                        }
                    })
                    .collect::<Vec<Device>>();
                    if let Err(e) = tx.send(Ok(DiscoverResponse{ devices })).await {
                        // TODO: consider re-registering here
                        error!("discover - for debugEcho failed to send discovery response with error {}", e);
                        break;
                    }
                }
                delay_for(Duration::from_secs(DISCOVERY_INTERVAL_SECS)).await;
            }
        });
        info!("outside of thread");
        Ok(Response::new(rx))
    }
}

fn get_configuration(
    discovery_details: &HashMap<String, String>,
)  -> Result<DebugEchoDiscoveryHandlerConfig, Error>{
    info!("inner_get_discovery_handler - for discovery details {:?}", discovery_details);
    // Determine whether it is an embedded protocol
    if let Some(discovery_handler_str) = discovery_details.get("protocolHandler") {
        info!("protocol handler {:?}",discovery_handler_str);
        if let Ok(discovery_handler) = serde_yaml::from_str(discovery_handler_str) {
            match discovery_handler {
                DiscoveryHandlerType::DebugEcho(debug_echo_discovery_handler_config) => Ok(debug_echo_discovery_handler_config),
                _ => Err(anyhow::format_err!("No protocol configured")),
            }
        } else {
            Err(anyhow::format_err!("Discovery details had protocol handler but does not have embedded support. Discovery details: {:?}", discovery_details))
        }
    } else {
        Err(anyhow::format_err!("Generic discovery handlers not supported. Discovery details: {:?}", discovery_details))
    }
}

pub async fn run_debug_echo_server(
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    info!("run_debug_echo_server - entered");
    let discovery_handler = DebugEchoDiscoveryHandler::new();
    let addr = DISCOVERY_ENDPOINT.parse()?;
    // TODO: when to shutdown? 
    Server::builder().add_service(DiscoveryServer::new(discovery_handler)).serve(addr).await?;
    info!("run_debug_echo_server - finished");
    Ok(())
}