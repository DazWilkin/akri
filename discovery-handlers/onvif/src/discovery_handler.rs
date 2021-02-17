use super::discovery_impl::util;
use super::discovery_utils::{
    OnvifQuery, OnvifQueryImpl, ONVIF_DEVICE_IP_ADDRESS_LABEL_ID,
    ONVIF_DEVICE_MAC_ADDRESS_LABEL_ID, ONVIF_DEVICE_SERVICE_URL_LABEL_ID,
};
use akri_discovery_utils::discovery::{
    v0::{discovery_server::Discovery, Device, DiscoverRequest, DiscoverResponse},
    DiscoverStream,
};
use akri_shared::akri::configuration::{FilterList, FilterType};
use anyhow::Error;
use async_trait::async_trait;
use log::{error, info, trace};
use std::{collections::HashMap, time::Duration};
use tokio::{sync::mpsc, time::delay_for};
use tonic::{Response, Status};

/// Protocol name that onvif discovery handlers use when registering with the Agent
pub const PROTOCOL_NAME: &str = "onvif";
/// Endpoint for the onvif discovery services if not using UDS
pub const DISCOVERY_PORT: &str = "10000";
// TODO: make this configurable
pub const DISCOVERY_INTERVAL_SECS: u64 = 10;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub enum DiscoveryHandlerType {
    Onvif(OnvifDiscoveryHandlerConfig),
}

/// This defines the ONVIF data stored in the Configuration
/// CRD
///
/// The ONVIF discovery handler is structured to store a filter list for
/// ip addresses, mac addresses, and ONVIF scopes.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OnvifDiscoveryHandlerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_addresses: Option<FilterList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac_addresses: Option<FilterList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<FilterList>,
    #[serde(default = "default_discovery_timeout_seconds")]
    pub discovery_timeout_seconds: i32,
}

fn default_discovery_timeout_seconds() -> i32 {
    1
}

/// `DiscoveryHandler` discovers the onvif instances as described by the filters `discover_handler_config.ip_addresses`,
/// `discover_handler_config.mac_addresses`, and `discover_handler_config.scopes`.
/// The instances it discovers are always shared.
pub struct DiscoveryHandler {
    shutdown_sender: Option<tokio::sync::mpsc::Sender<()>>,
}

impl DiscoveryHandler {
    pub fn new(shutdown_sender: Option<tokio::sync::mpsc::Sender<()>>) -> Self {
        DiscoveryHandler { shutdown_sender }
    }
}

#[async_trait]
impl Discovery for DiscoveryHandler {
    type DiscoverStream = DiscoverStream;
    async fn discover(
        &self,
        request: tonic::Request<DiscoverRequest>,
    ) -> Result<Response<Self::DiscoverStream>, Status> {
        info!("discover - called for ONVIF protocol");
        let shutdown_sender = self.shutdown_sender.clone();
        let discover_request = request.get_ref();
        let (mut tx, rx) = mpsc::channel(4);
        let discovery_handler_config = deserialize_discovery_details(&discover_request.discovery_details)
            .map_err(|e| {
                tonic::Status::new(
                    tonic::Code::InvalidArgument,
                    format!("Invalid ONVIF discovery handler configuration: {}", e),
                )
            })?;
        let mut cameras: Vec<Device> = Vec::new();
        tokio::spawn(async move {
            loop {
                let onvif_query = OnvifQueryImpl {};

                trace!("discover - filters:{:?}", &discovery_handler_config,);
                let discovered_onvif_cameras = util::simple_onvif_discover(Duration::from_secs(
                    discovery_handler_config.discovery_timeout_seconds as u64,
                ))
                .await
                .unwrap();
                trace!("discover - discovered:{:?}", &discovered_onvif_cameras,);
                // apply_filters never returns an error -- safe to unwrap
                let filtered_onvif_cameras = apply_filters(
                    &discovery_handler_config,
                    discovered_onvif_cameras,
                    &onvif_query,
                )
                .await
                .unwrap();
                trace!("discover - filtered:{:?}", &filtered_onvif_cameras);
                let mut changed_camera_list = false;
                let mut matching_camera_count = 0;
                filtered_onvif_cameras.iter().for_each(|camera| {
                    if !cameras.contains(camera) {
                        changed_camera_list = true;
                    } else {
                        matching_camera_count += 1;
                    }
                });
                if changed_camera_list || matching_camera_count != cameras.len() {
                    trace!("discover - sending updated device list");
                    cameras = filtered_onvif_cameras.clone();
                    if let Err(e) = tx
                        .send(Ok(DiscoverResponse {
                            devices: filtered_onvif_cameras,
                        }))
                        .await
                    {
                        error!(
                            "discover - for ONVIF failed to send discovery response with error {}",
                            e
                        );
                        if let Some(mut sender) = shutdown_sender {
                            sender.send(()).await.unwrap();
                        }
                        break;
                    }
                }
                delay_for(Duration::from_secs(DISCOVERY_INTERVAL_SECS)).await;
            }
        });
        Ok(Response::new(rx))
    }
}

fn execute_filter(filter_list: Option<&FilterList>, filter_against: &[String]) -> bool {
    if filter_list.is_none() {
        return false;
    }
    let filter_action = filter_list.as_ref().unwrap().action.clone();
    let filter_count = filter_list
        .unwrap()
        .items
        .iter()
        .filter(|pattern| {
            filter_against
                .iter()
                .filter(|filter_against_item| filter_against_item.contains(*pattern))
                .count()
                > 0
        })
        .count();

    if FilterType::Include == filter_action {
        filter_count == 0
    } else {
        filter_count != 0
    }
}

async fn apply_filters(
    discovery_handler_config: &OnvifDiscoveryHandlerConfig,
    device_service_uris: Vec<String>,
    onvif_query: &impl OnvifQuery,
) -> Result<Vec<Device>, anyhow::Error> {
    let mut result = Vec::new();
    for device_service_url in device_service_uris.iter() {
        trace!("apply_filters - device service url {}", &device_service_url);
        let (ip_address, mac_address) = match onvif_query
            .get_device_ip_and_mac_address(&device_service_url)
            .await
        {
            Ok(ip_and_mac) => ip_and_mac,
            Err(e) => {
                error!("apply_filters - error getting ip and mac address: {}", e);
                continue;
            }
        };

        // Evaluate camera ip address against ip filter if provided
        let ip_address_as_vec = vec![ip_address.clone()];
        if execute_filter(
            discovery_handler_config.ip_addresses.as_ref(),
            &ip_address_as_vec,
        ) {
            continue;
        }

        // Evaluate camera mac address against mac filter if provided
        let mac_address_as_vec = vec![mac_address.clone()];
        if execute_filter(
            discovery_handler_config.mac_addresses.as_ref(),
            &mac_address_as_vec,
        ) {
            continue;
        }

        let ip_and_mac_joined = format!("{}-{}", &ip_address, &mac_address);

        // Evaluate camera scopes against scopes filter if provided
        let device_scopes = match onvif_query.get_device_scopes(&device_service_url).await {
            Ok(scopes) => scopes,
            Err(e) => {
                error!("apply_filters - error getting scopes: {}", e);
                continue;
            }
        };
        if execute_filter(discovery_handler_config.scopes.as_ref(), &device_scopes) {
            continue;
        }

        let mut properties = HashMap::new();
        properties.insert(
            ONVIF_DEVICE_SERVICE_URL_LABEL_ID.to_string(),
            device_service_url.to_string(),
        );
        properties.insert(ONVIF_DEVICE_IP_ADDRESS_LABEL_ID.into(), ip_address);
        properties.insert(ONVIF_DEVICE_MAC_ADDRESS_LABEL_ID.into(), mac_address);

        trace!(
            "apply_filters - returns DiscoveryResult ip/mac: {:?}, props: {:?}",
            &ip_and_mac_joined,
            &properties
        );
        result.push(Device {
            id: ip_and_mac_joined,
            properties,
            mounts: Vec::default(),
            device_specs: Vec::default(),
        })
    }
    Ok(result)
}

/// This obtains the `OnvifDiscoveryHandlerConfig` from a discovery details map.
/// It expects the `OnvifDiscoveryHandlerConfig` to be serialized yaml stored in the map as
/// the String value associated with the key `protocolHandler`.
fn deserialize_discovery_details(
    discovery_details: &HashMap<String, String>,
) -> Result<OnvifDiscoveryHandlerConfig, Error> {
    trace!(
        "inner_get_discovery_handler - for discovery details {:?}",
        discovery_details
    );
    // Determine whether it is an embedded protocol
    if let Some(discovery_handler_str) = discovery_details.get("protocolHandler") {
        trace!("protocol handler {:?}", discovery_handler_str);
        if let Ok(discovery_handler) = serde_yaml::from_str(discovery_handler_str) {
            match discovery_handler {
                DiscoveryHandlerType::Onvif(discovery_handler_config) => {
                    Ok(discovery_handler_config)
                }
            }
        } else {
            Err(anyhow::format_err!("Discovery details had protocol handler but does not have embedded support. Discovery details: {:?}", discovery_details))
        }
    } else {
        Err(anyhow::format_err!(
            "Generic discovery handlers not supported. Discovery details: {:?}",
            discovery_details
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_discovery_details() {
        let onvif_yaml = r#"
          protocolHandler: |+
            onvif: {}
        "#;
        let deserialized: HashMap<String, String> = serde_yaml::from_str(&onvif_yaml).unwrap();
        let serialized = serde_json::to_string(&deserialize_discovery_details(&deserialized).unwrap()).unwrap();
        let expected_deserialized = r#"{"discoveryTimeoutSeconds":1}"#;
        assert_eq!(expected_deserialized, serialized);
    }
}