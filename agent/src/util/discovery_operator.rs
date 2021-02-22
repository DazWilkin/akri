use super::super::INSTANCE_COUNT_METRIC;
use super::{
    constants::SHARED_INSTANCE_OFFLINE_GRACE_PERIOD_SECS,
    device_plugin_builder::{DevicePluginBuilder, DevicePluginBuilderInterface},
    device_plugin_service,
    device_plugin_service::{
        get_device_instance_name, InstanceConnectivityStatus, InstanceInfo, InstanceMap,
    },
    embedded_discovery_handlers::get_discovery_handler,
    registration::{
        DiscoveryHandlerConnectivityStatus, DiscoveryHandlerDetails, RegisteredDiscoveryHandlerMap,
        DISCOVERY_HANDLER_OFFLINE_GRACE_PERIOD_SECS, EMBEDDED_DISCOVERY_HANDLER_ENDPOINT,
    },
    streaming_extension::StreamingExt,
};
use akri_discovery_utils::discovery::v0::{
    discovery_client::DiscoveryClient, Device, DiscoverRequest, DiscoverResponse,
};
use akri_shared::{
    akri::configuration::KubeAkriConfig,
    k8s,
    os::env_var::{ActualEnvVarQuery, EnvVarQuery},
};
use blake2::{
    digest::{Update, VariableOutput},
    VarBlake2b,
};
use log::{error, trace};
#[cfg(test)]
use mockall::{automock, predicate::*};
use std::{collections::HashMap, convert::TryFrom, sync::Arc, time::Instant};
use tokio::sync::{broadcast, mpsc};
use tonic::{
    transport::{Endpoint, Uri},
    Status,
};

/// StreamType provides a wrapper around the two different types of streams returned from embedded
/// or internal discovery handlers and ones running externally.
pub enum StreamType {
    Internal(mpsc::Receiver<std::result::Result<DiscoverResponse, Status>>),
    External(tonic::Streaming<DiscoverResponse>),
}

/// A DiscoveryOperator is created for each Configuration that is applied to the cluster.
/// It handles discovery of the devices specified in a Configuration by calling `Discover` on
/// all registered discovery handlers that are using the same protocol as specified in `Configuration.protocol.name.`
/// For each device discovered by the discovery handlers, it creates a device plugin.
/// If a device disappears, it deletes the associated instance after a grace period (for non-local devices).
/// Note: Since this structure is automocked, the compiler does not seem to be able to confirm that all the
/// methods are being used. Therefore, #[allow(dead_code)] has been added to all methods that are not invoked or
/// tested on a DiscoveryOperator.
#[derive(Clone)]
pub struct DiscoveryOperator {
    discovery_handler_map: RegisteredDiscoveryHandlerMap,
    config: KubeAkriConfig,
    instance_map: InstanceMap,
    device_plugin_path: String,
}

#[cfg_attr(test, automock)]
impl DiscoveryOperator {
    pub fn new(
        discovery_handler_map: RegisteredDiscoveryHandlerMap,
        config: KubeAkriConfig,
        instance_map: InstanceMap,
        device_plugin_path: String,
    ) -> Self {
        DiscoveryOperator {
            discovery_handler_map,
            config,
            instance_map,
            device_plugin_path,
        }
    }
    /// Returns discovery_handler_map field. Allows the struct to be mocked.
    #[allow(dead_code)]
    pub fn get_discovery_handler_map(&self) -> RegisteredDiscoveryHandlerMap {
        self.discovery_handler_map.clone()
    }
    /// Returns config field. Allows the struct to be mocked.
    #[allow(dead_code)]
    pub fn get_config(&self) -> KubeAkriConfig {
        self.config.clone()
    }
    /// Returns instance_map field. Allows the struct to be mocked.
    #[allow(dead_code)]
    pub fn get_instance_map(&self) -> InstanceMap {
        self.instance_map.clone()
    }
    /// Returns device_plugin_path field. Allows the struct to be mocked.
    #[allow(dead_code)]
    pub fn get_device_plugin_path(&self) -> String {
        self.device_plugin_path.clone()
    }
    #[allow(dead_code)]
    pub async fn stop_all_discovery(&self) {
        let mut discovery_handler_map = self.discovery_handler_map.lock().unwrap().clone();
        if let Some(protocol_dhs_map) =
            discovery_handler_map.get_mut(&self.config.spec.protocol.name)
        {
            for (endpoint, dh_details) in protocol_dhs_map.clone() {
                match dh_details.stop_discovery.send(()) {
                    Ok(_) => trace!("stop_all_discovery - discovery client for protocol {} at endpoint {} told to stop", self.config.spec.protocol.name, endpoint),
                    Err(e) => error!("stop_all_discovery - discovery client for protocol {} at endpoint {} could not receive stop message with error {:?}", self.config.spec.protocol.name, endpoint, e)
                }
            }
        }
    }

    /// Calls discover on the Discovery Handler at the given endpoint and returns the connection stream.
    pub async fn get_stream(
        &self,
        endpoint: &str,
        discovery_handler_path: &str,
    ) -> Option<StreamType> {
        let discover_request = tonic::Request::new(DiscoverRequest {
            discovery_details: self.config.spec.protocol.discovery_details.clone(),
        });
        trace!("get_stream - endpoint is {}", endpoint);
        if endpoint == EMBEDDED_DISCOVERY_HANDLER_ENDPOINT
            && get_discovery_handler(&self.config.spec.protocol.discovery_details).is_ok()
        {
            trace!(
                "get_stream - internal discovery handler for protocol {}",
                self.config.spec.protocol.name
            );
            let discovery_handler =
                get_discovery_handler(&self.config.spec.protocol.discovery_details).unwrap();
            Some(StreamType::Internal(
                discovery_handler
                    .discover(discover_request)
                    .await
                    .unwrap()
                    .into_inner(),
            ))
        // Check if is a UDS connection
        } else if endpoint.starts_with(discovery_handler_path) {
            let path = endpoint.to_string();
            match Endpoint::try_from("lttp://[::]:50051")
                .unwrap()
                .connect_with_connector(tower::service_fn(move |_: Uri| {
                    let endpoint = path.clone();
                    tokio::net::UnixStream::connect(endpoint)
                }))
                .await
            {
                Ok(channel) => {
                    trace!(
                        "get_stream - external discovery handler for protocol {}",
                        self.config.spec.protocol.name
                    );
                    let mut discovery_client = DiscoveryClient::new(channel);
                    Some(StreamType::External(
                        discovery_client
                            .discover(discover_request)
                            .await
                            .unwrap()
                            .into_inner(),
                    ))
                }
                Err(e) => {
                    error!("get_stream - failed to connect to client with error {}", e);
                    None
                }
            }
        } else {
            match DiscoveryClient::connect(endpoint.to_string()).await {
                Ok(mut discovery_client) => {
                    trace!(
                        "get_stream - external discovery handler for protocol {}",
                        self.config.spec.protocol.name
                    );
                    Some(StreamType::External(
                        discovery_client
                            .discover(discover_request)
                            .await
                            .unwrap()
                            .into_inner(),
                    ))
                }
                Err(e) => {
                    error!("get_stream - failed to connect to client with error {}", e);
                    None
                }
            }
        }
    }
    /// Listens for new discovery responses or a notification to stop connecting to the Discovery Handler
    #[allow(dead_code)]
    pub async fn internal_do_discover<'a>(
        &'a self,
        kube_interface: Arc<Box<dyn k8s::KubeInterface>>,
        endpoint: &'a str,
        dh_details: &'a DiscoveryHandlerDetails,
        stream: &'a mut dyn StreamingExt,
    ) -> Result<(), Status> {
        // clone objects for thread
        let discovery_operator = Arc::new(self.clone());
        let stop_discovery_receiver: &mut broadcast::Receiver<()> =
            &mut dh_details.stop_discovery.subscribe();
        loop {
            // Wait for either new discovery results or a message to stop discovery
            tokio::select! {
                _ = try_receive(stop_discovery_receiver) => {
                    trace!("internal_do_discover - received message to stop discovery for endpoint {} serving protocol {}", endpoint, discovery_operator.config.spec.protocol.name);
                    break;
                },
                result = stream.get_message() => {
                    let message = result?;
                    if let Some(response) = message {
                        trace!("internal_do_discover - got discovery results {:?}", response.devices);
                        self.handle_discovery_results(
                            kube_interface.clone(),
                            response.devices,
                            dh_details.register_request.is_local,
                            Box::new(DevicePluginBuilder{}),
                        )
                        .await
                        .unwrap();
                    } else {
                        error!("internal_do_discover - received result of type None. Should not happen.");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Sets the connectivity status of a discovery handler. If a discovery handler goes offline, mark_offline_or_deregister_discovery_handler should be used.
    pub fn set_discovery_handler_connectivity_status(
        &self,
        endpoint: &str,
        connectivity_status: DiscoveryHandlerConnectivityStatus,
    ) -> () {
        trace!("set_discovery_handler_connectivity_status - discovery handler at endpoint {} and protocol {} is offline", endpoint, self.config.spec.protocol.name);
        if let Some(registered_dh_map) = self
            .discovery_handler_map
            .lock()
            .unwrap()
            .get_mut(&self.config.spec.protocol.name)
        {
            if let Some(dh_details) = registered_dh_map.get(endpoint) {
                let mut dh_details = dh_details.clone();
                dh_details.connectivity_status = connectivity_status;
                registered_dh_map.insert(endpoint.to_string(), dh_details);
            }
        }
    }

    /// This is called when no connection can be made with a discovery handler at its endpoint. The discovery handler should be marked offline or removed
    /// from the RegisteredDiscoveryHandlersMap if it has timed out or its grace period has passed. Will never be
    pub async fn mark_offline_or_deregister_discovery_handler(
        &self,
        endpoint: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync + 'static>> {
        trace!("mark_offline_or_deregister_discovery_handler - discovery handler at endpoint {} and protocol {} is offline", endpoint, self.config.spec.protocol.name);
        let mut deregistered = false;
        if let Some(registered_dh_map) = self
            .discovery_handler_map
            .lock()
            .unwrap()
            .get_mut(&self.config.spec.protocol.name)
        {
            if let Some(dh_details) = registered_dh_map.get(endpoint) {
                match dh_details.connectivity_status {
                    DiscoveryHandlerConnectivityStatus::Offline(instant) => {
                        if instant.elapsed().as_secs() > DISCOVERY_HANDLER_OFFLINE_GRACE_PERIOD_SECS
                        {
                            trace!("mark_offline_or_deregister_discovery_handler - de-registering discovery handler for protocol {} at endpoint {} since been offline for longer than 5 minutes", self.config.spec.protocol.name, endpoint);
                            // Remove discovery handler from map if timed out
                            registered_dh_map.remove(endpoint);
                            deregistered = true;
                        }
                    }
                    DiscoveryHandlerConnectivityStatus::Online => {
                        let mut dh_details = dh_details.clone();
                        dh_details.connectivity_status =
                            DiscoveryHandlerConnectivityStatus::Offline(Instant::now());
                        registered_dh_map.insert(endpoint.to_string(), dh_details);
                    }
                    DiscoveryHandlerConnectivityStatus::HasClient => {}
                }
            }
        }
        Ok(deregistered)
    }

    /// Checks if any of this DiscoveryOperator's Configuration's Instances have been offline for too long.
    /// If a non-local device has not come back online before `SHARED_INSTANCE_OFFLINE_GRACE_PERIOD_SECS`,
    /// the associated device plugin and instance are terminated and deleted.
    #[allow(dead_code)]
    pub async fn check_offline_status(
        &self,
        kube_interface: Arc<Box<dyn k8s::KubeInterface>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        trace!(
            "check_offline_status - entered for configuration {}",
            self.config.metadata.name
        );
        let kube_interface_clone = kube_interface.clone();
        let instance_map = self.instance_map.lock().await.clone();
        for (instance, instance_info) in instance_map.clone() {
            match instance_info.connectivity_status {
                InstanceConnectivityStatus::Online => {}
                InstanceConnectivityStatus::Offline(instant) => {
                    let time_offline = instant.elapsed().as_secs();
                    // If instance has been offline for longer than the grace period or it is unshared, terminate the associated device plugin
                    // TODO: make grace period configurable
                    if time_offline >= SHARED_INSTANCE_OFFLINE_GRACE_PERIOD_SECS {
                        trace!("check_offline_status - instance {} has been offline too long ... terminating device plugin", instance);
                        device_plugin_service::terminate_device_plugin_service(
                            &instance,
                            self.instance_map.clone(),
                        )
                        .await
                        .unwrap();
                        k8s::try_delete_instance_arc(
                            kube_interface_clone.clone(),
                            instance.clone(),
                            self.config.metadata.namespace.as_ref().unwrap().clone(),
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Takes in a list of discovered devices and determines if there are any new devices or no longer visible devices.
    /// For each new device, it creates a DevicePluginService.
    /// For each previously visible device that was no longer discovered, it calls a function that updates the InstanceConnectivityStatus
    /// of the instance or deletes it if it is a local device.
    pub async fn handle_discovery_results(
        &self,
        kube_interface: Arc<Box<dyn k8s::KubeInterface>>,
        discovery_results: Vec<Device>,
        is_local: bool,
        device_plugin_builder: Box<dyn DevicePluginBuilderInterface>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        trace!(
            "handle_discovery_results - for config {} with discovery results {:?}",
            self.config.metadata.name,
            discovery_results
        );
        let currently_visible_instances: HashMap<String, Device> = discovery_results
            .iter()
            .map(|discovery_result| {
                let id = generate_instance_digest(&discovery_result.id, !is_local);
                let instance_name = get_device_instance_name(&id, &self.config.metadata.name);
                (instance_name, discovery_result.clone())
            })
            .collect();
        INSTANCE_COUNT_METRIC
            .with_label_values(&[&self.config.metadata.name, &is_local.to_string()])
            .set(currently_visible_instances.len() as i64);
        // Update the connectivity status of instances and return list of visible instances that don't have Instance CRs
        let instance_map = self.instance_map.lock().await.clone();
        // Find all visible instances that do not have Instance CRDs yet
        let new_discovery_results: Vec<Device> = currently_visible_instances
            .iter()
            .filter(|(name, _)| !instance_map.contains_key(*name))
            .map(|(_, p)| p.clone())
            .collect();
        self.update_connectivity_status(kube_interface, currently_visible_instances, is_local)
            .await?;

        // If there are newly visible instances associated with a Config, make a device plugin and Instance CR for them
        if !new_discovery_results.is_empty() {
            for discovery_result in new_discovery_results {
                let id = generate_instance_digest(&discovery_result.id, !is_local);
                let instance_name = get_device_instance_name(&id, &self.config.metadata.name);
                trace!(
                    "handle_discovery_results - new instance {} came online",
                    instance_name
                );
                let config_spec = self.config.spec.clone();
                let instance_map = self.instance_map.clone();
                if let Err(e) = device_plugin_builder
                    .build_device_plugin(
                        instance_name,
                        self.config.metadata.name.clone(),
                        self.config.metadata.uid.as_ref().unwrap().clone(),
                        self.config.metadata.namespace.as_ref().unwrap().clone(),
                        config_spec,
                        !is_local,
                        instance_map,
                        &self.device_plugin_path,
                        discovery_result.clone(),
                    )
                    .await
                {
                    error!("handle_discovery_results - error {} building device plugin ... trying again on next iteration", e);
                }
            }
        }
        Ok(())
    }

    /// Takes in a list of currently visible instances and either updates an Instance's InstanceConnectivityStatus or deletes an Instance.
    /// If a non-local/network based device is not longer visible it's InstanceConnectivityStatus is changed to Offline(time now).
    /// The associated DevicePluginService checks its InstanceConnectivityStatus before sending a response back to kubelet
    /// and will send all unhealthy devices if its status is Offline, preventing kubelet from allocating any more pods to it.
    /// An Instance CRD is deleted and it's DevicePluginService shutdown if its:
    /// (A) non-local Instance is still not visible after 5 minutes or (B) local instance is still not visible.
    pub async fn update_connectivity_status(
        &self,
        kube_interface: Arc<Box<dyn k8s::KubeInterface>>,
        currently_visible_instances: HashMap<String, Device>,
        is_local: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        let instance_map = self.instance_map.lock().await.clone();
        for (instance, instance_info) in instance_map {
            trace!(
                "update_connectivity_status - checking connectivity status of instance {}",
                instance
            );
            if currently_visible_instances.contains_key(&instance) {
                let connectivity_status = instance_info.connectivity_status;
                // If instance is visible, make sure connectivity status is (updated to be) Online
                if let InstanceConnectivityStatus::Offline(_instant) = connectivity_status {
                    trace!(
                        "update_connectivity_status - instance {} that was temporarily offline is back online",
                        instance
                    );
                    let list_and_watch_message_sender = instance_info.list_and_watch_message_sender;
                    let updated_instance_info = InstanceInfo {
                        connectivity_status: InstanceConnectivityStatus::Online,
                        list_and_watch_message_sender: list_and_watch_message_sender.clone(),
                    };
                    self.instance_map
                        .lock()
                        .await
                        .insert(instance.clone(), updated_instance_info);
                    // Signal list_and_watch to update kubelet that the devices are healthy.
                    list_and_watch_message_sender
                        .send(device_plugin_service::ListAndWatchMessageKind::Continue)
                        .unwrap();
                } else {
                    trace!(
                        "update_connectivity_status - instance {} still online",
                        instance
                    );
                }
            } else {
                // If the instance is not visible:
                // // If the instance is local, remove it
                // // If the instance is not local
                // // // If it has not already been labeled offline, label it
                // // // If the instance has already been labeled offline
                // // // remove instance from map if grace period has elapsed without the instance coming back online
                let mut remove_instance = false;
                match instance_info.connectivity_status {
                    InstanceConnectivityStatus::Online => {
                        if is_local {
                            remove_instance = true;
                        } else {
                            let sender = instance_info.list_and_watch_message_sender.clone();
                            let updated_instance_info = InstanceInfo {
                                connectivity_status: InstanceConnectivityStatus::Offline(
                                    Instant::now(),
                                ),
                                list_and_watch_message_sender: instance_info
                                    .list_and_watch_message_sender
                                    .clone(),
                            };
                            self.instance_map
                                .lock()
                                .await
                                .insert(instance.clone(), updated_instance_info);
                            trace!(
                                "update_connectivity_status - instance {} went offline ... starting timer and forcing list_and_watch to continue",
                                instance
                            );
                            sender
                                .send(device_plugin_service::ListAndWatchMessageKind::Continue)
                                .unwrap();
                        }
                    }
                    InstanceConnectivityStatus::Offline(instant) => {
                        let time_offline = instant.elapsed().as_secs();
                        // If instance has been offline for longer than the grace period, terminate the associated device plugin
                        if time_offline >= SHARED_INSTANCE_OFFLINE_GRACE_PERIOD_SECS {
                            remove_instance = true;
                        }
                    }
                }
                if remove_instance {
                    trace!("update_connectivity_status - instance {} has been offline too long ... terminating device plugin", instance);
                    device_plugin_service::terminate_device_plugin_service(
                        &instance,
                        self.instance_map.clone(),
                    )
                    .await
                    .unwrap();
                    k8s::try_delete_instance_arc(
                        kube_interface.clone(),
                        instance.clone(),
                        self.config.metadata.namespace.as_ref().unwrap().clone(),
                    )
                    .await
                    .unwrap();
                }
            }
        }
        Ok(())
    }
}

pub mod start_discovery {
    use super::super::registration::DiscoveryHandlerConnectivityStatus;
    #[double]
    pub use super::DiscoveryOperator;
    use super::{try_receive, StreamType};
    use akri_discovery_utils::discovery::DISCOVERY_HANDLER_PATH;
    use akri_shared::k8s;
    use mockall_double::double;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::broadcast;

    /// This is spawned as a task for each Configuration and continues to run
    /// until the Configuration is deleted, at which point, this function is signaled to stop.
    /// In a separate task, it calls connects to each discovery handler in the RegisteredDiscoveryHandlerMap
    /// with the same protocol name as the Configuration (Configuration.protocol.name). Then, it listens for
    /// updates from the discovery handler on what devices are currently visible to the node.
    /// Passes this list to a function that updates the InstanceConnectivityStatus of the Configuration's Instances
    /// or deletes Instance CRs if needed. If a new instance becomes visible that isn't in the Configuration's InstanceMap,
    /// a DevicePluginService and Instance CRD are created for it, and it is added to the InstanceMap.
    ///
    /// It also spawns a task to check whether Offline Instances have exceeded their grace period, in which case it
    /// deletes the Instance.
    pub async fn start_discovery(
        discovery_operator: DiscoveryOperator,
        new_discovery_handler_sender: broadcast::Sender<String>,
        stop_all_discovery_sender: broadcast::Sender<()>,
        finished_all_discovery_sender: &mut broadcast::Sender<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        let config = discovery_operator.get_config();
        info!(
            "start_discovery - entered for protocol {}",
            config.spec.protocol.name
        );
        let config_name = config.metadata.name.clone();
        let mut tasks = Vec::new();
        let discovery_operator = Arc::new(discovery_operator);
        let task1_discovery_operator = discovery_operator.clone();
        // Call discover on already registered Discovery Handlers for this Configuration's protocol
        tasks.push(tokio::spawn(async move {
            do_discover(
                task1_discovery_operator,
                Arc::new(Box::new(k8s::create_kube_interface())),
            )
            .await
            .unwrap();
        }));
        let mut stop_all_discovery_receiver = stop_all_discovery_sender.subscribe();
        let mut new_discovery_handler_receiver = new_discovery_handler_sender.subscribe();
        let task2_discovery_operator = discovery_operator.clone();
        tasks.push(tokio::spawn(async move {
            let mut inner_tasks = Vec::new();
            loop {
                tokio::select! {
                    _ = try_receive(&mut stop_all_discovery_receiver) => {
                        trace!("start_discovery - received message to stop discovery for configuration {}", task2_discovery_operator.get_config().metadata.name);
                        // stop_offline_checks_sender.send(()).unwrap();
                        task2_discovery_operator.stop_all_discovery().await;
                        break;
                    },
                    result = try_receive(&mut new_discovery_handler_receiver) => {
                        // check if it is this protocol
                        if let Ok(protocol) = result {
                            if protocol == task2_discovery_operator.get_config().spec.protocol.name {
                                trace!("start_discovery - received new registered discovery handler for configuration {}", task2_discovery_operator.get_config().metadata.name);
                                let new_discovery_operator = task2_discovery_operator.clone();
                                inner_tasks.push(tokio::spawn(async move {
                                    do_discover(new_discovery_operator, Arc::new(Box::new(k8s::create_kube_interface()))).await.unwrap();
                                }));
                            }
                        }
                    }
                }
            }
            futures::future::try_join_all(inner_tasks).await.unwrap();
        }));
        let mut stop_all_discovery_receiver = stop_all_discovery_sender.subscribe();
        let task3_discovery_operator = discovery_operator.clone();
        // Non-local devices are only allowed to be offline for `SHARED_INSTANCE_OFFLINE_GRACE_PERIOD_SECS` minutes before being removed.
        // This task periodically checks if devices have been offline for too long.
        tasks.push(tokio::spawn(async move {
            let kube_interface: Arc<Box<dyn k8s::KubeInterface>> = Arc::new(Box::new(k8s::create_kube_interface()));
            loop {
                task3_discovery_operator
                    .check_offline_status(kube_interface.clone())
                    .await
                    .unwrap();
                if tokio::time::timeout(
                    Duration::from_secs(30),
                    stop_all_discovery_receiver.recv(),
                )
                .await.is_ok()
                {
                    trace!("start_discovery - received message to stop checking connectivity status for configuration {}", config_name);
                    break;
                }
            }
        }));
        futures::future::try_join_all(tasks).await?;
        finished_all_discovery_sender.send(()).unwrap();
        Ok(())
    }

    /// For each Discovery Handler registered for this DiscoveryOperator's protocol,
    /// tries to establish connection with the DiscoveryHandler and spawns a discovery thread for each connection.
    /// This function also manages the DiscoveryHandlerConnectivityStatus of each Discovery Handler as follows:
    /// /// DiscoveryHandlerConnectivityStatus::HasClient if a connection is established via a call to get_stream
    /// /// DiscoveryHandlerConnectivityStatus::Online after a connection has finished due to either being signaled to stop connecting
    /// /// or an error being returned from the discovery handler (that is not a broken pipe)
    /// /// DiscoveryHandlerConnectivityStatus::Offline if a connection cannot be established via a call to get_stream
    /// If a connection cannot be established, continues to try, sleeping between iteration.
    /// Removes the discovery handler from the RegisteredDiscoveryHandlerMap if it has been offline for longer than the grace period.

    pub async fn do_discover(
        discovery_operator: Arc<DiscoveryOperator>,
        kube_interface: Arc<Box<dyn k8s::KubeInterface>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        let config = discovery_operator.get_config();
        trace!(
            "do_discover - entered for protocol {}",
            config.spec.protocol.name
        );
        let mut tasks = Vec::new();
        let mut discovery_operator = Arc::new(discovery_operator.clone());
        // get clone of map
        let mut discovery_handler_map = discovery_operator
            .get_discovery_handler_map()
            .lock()
            .unwrap()
            .clone();
        trace!(
            "do_discover - discovery_handler_map is {:?}",
            discovery_handler_map
        );
        if let Some(protocol_dhs_map) = discovery_handler_map.get_mut(&config.spec.protocol.name) {
            for (endpoint, dh_details) in protocol_dhs_map.clone() {
                trace!(
                    "do_discover - for protocol {} and endpoint {}",
                    config.spec.protocol.name,
                    endpoint
                );
                discovery_operator = discovery_operator.clone();
                // Check if there is already a client
                if dh_details.connectivity_status != DiscoveryHandlerConnectivityStatus::HasClient {
                    trace!(
                        "do_discover - endpoint {} for protocol {} doesn't have client",
                        endpoint,
                        config.spec.protocol.name
                    );
                    let discovery_operator = discovery_operator.clone();
                    let kube_interface = kube_interface.clone();
                    tasks.push(tokio::spawn(async move {
                        // Try to connect to discovery handler until connection has been established or grace period has passed
                        loop {
                            let deregistered;
                            match discovery_operator.get_stream(&endpoint, DISCOVERY_HANDLER_PATH).await {
                                Some(stream_type) => {
                                    // Since connection was established, be sure that the Discovery Handler is marked as having a client
                                    discovery_operator.set_discovery_handler_connectivity_status(&endpoint, DiscoveryHandlerConnectivityStatus::HasClient);
                                    match stream_type {
                                        StreamType::External(mut stream) => {
                                            match discovery_operator.internal_do_discover(kube_interface.clone(), &endpoint, &dh_details, &mut stream).await {
                                                Ok(_) => {
                                                    discovery_operator.set_discovery_handler_connectivity_status(&endpoint, DiscoveryHandlerConnectivityStatus::Online);
                                                    break;
                                                },
                                                Err(status) => {
                                                    if status.message().contains("broken pipe") {
                                                        deregistered = discovery_operator.mark_offline_or_deregister_discovery_handler(&endpoint).await.unwrap();
                                                    } else {
                                                        trace!("do_discover - Discovery Handlers returned error status {}", status);
                                                        // TODO: Check for config error
                                                        discovery_operator.set_discovery_handler_connectivity_status(&endpoint, DiscoveryHandlerConnectivityStatus::Online);
                                                        break;
                                                    }
                                                }
                                            }
                                        },
                                        StreamType::Internal(mut stream) => {
                                            discovery_operator.internal_do_discover(kube_interface.clone(), &endpoint, &dh_details, &mut stream).await.unwrap();
                                            discovery_operator.set_discovery_handler_connectivity_status(&endpoint, DiscoveryHandlerConnectivityStatus::Online);
                                            break;
                                        }
                                    }
                                },
                                None => {
                                    deregistered = discovery_operator.mark_offline_or_deregister_discovery_handler(&endpoint).await.unwrap();
                                },
                            }
                            if deregistered {
                                break;
                            } else {
                                // Sleep and keep looping until connection established or deregistered due to grace period elapsing
                                #[cfg(not(test))]
                                tokio::time::delay_for(Duration::from_secs(60)).await;
                            }
                        }
                    }));
                }
            }
        }
        futures::future::try_join_all(tasks).await?;
        Ok(())
    }
}
async fn try_receive<T>(
    receiver: &mut broadcast::Receiver<T>,
) -> Result<T, tokio::sync::broadcast::RecvError>
where
    T: std::clone::Clone,
{
    receiver.recv().await
}

/// Generates an digest of an Instance's id. There should be a unique digest and Instance for each discovered device.
/// This means that the id of non-local devices that could be visible to multiple nodes should always resolve
/// to the same instance name (which is suffixed with this digest).
/// However, local devices' Instances should have unique hashes even if they have the same id.
/// To ensure this, the node's name is added to the id before it is hashed.
pub fn generate_instance_digest(id_to_digest: &str, shared: bool) -> String {
    let env_var_query = ActualEnvVarQuery {};
    inner_generate_instance_digest(id_to_digest, shared, &env_var_query)
}

pub fn inner_generate_instance_digest(
    id_to_digest: &str,
    shared: bool,
    query: &impl EnvVarQuery,
) -> String {
    let mut id_to_digest = id_to_digest.to_string();
    // For local devices, include node hostname in id_to_digest so instances have unique names
    if !shared {
        id_to_digest = format!(
            "{}{}",
            &id_to_digest,
            query.get_env_var("AGENT_NODE_NAME").unwrap()
        );
    }
    let mut digest = String::new();
    let mut hasher = VarBlake2b::new(3).unwrap();
    hasher.update(id_to_digest);
    hasher.finalize_variable(|var| {
        digest = var
            .iter()
            .map(|num| format!("{:02x}", num))
            .collect::<Vec<String>>()
            .join("")
    });
    digest
}

#[cfg(test)]
pub mod tests {
    use super::super::{
        device_plugin_builder::MockDevicePluginBuilderInterface,
        registration::{
            register_embedded_discovery_handlers, DiscoveryHandlerConnectivityStatus,
            DiscoveryHandlerDetails,
        },
    };
    use super::*;
    use akri_discovery_utils::discovery::v0::RegisterRequest;
    use akri_shared::{
        akri::configuration::KubeAkriConfig, k8s::MockKubeInterface, os::env_var::MockEnvVarQuery,
    };
    use mockall::Sequence;
    use std::time::Duration;
    use tempfile::Builder;

    pub async fn build_instance_map(
        config: &KubeAkriConfig,
        visible_discovery_results: &mut Vec<Device>,
        list_and_watch_message_receivers: &mut Vec<
            broadcast::Receiver<device_plugin_service::ListAndWatchMessageKind>,
        >,
        connectivity_status: InstanceConnectivityStatus,
    ) -> InstanceMap {
        let device1 = Device {
            id: "filter1".to_string(),
            properties: HashMap::new(),
            mounts: Vec::default(),
            device_specs: Vec::default(),
        };
        let device2 = Device {
            id: "filter2".to_string(),
            properties: HashMap::new(),
            mounts: Vec::default(),
            device_specs: Vec::default(),
        };
        let discovery_results = vec![device1, device2];
        *visible_discovery_results = discovery_results.clone();
        generate_instance_map(
            discovery_results,
            list_and_watch_message_receivers,
            connectivity_status,
            &config.metadata.name,
        )
    }

    fn generate_instance_map(
        discovery_results: Vec<Device>,
        list_and_watch_message_receivers: &mut Vec<
            broadcast::Receiver<device_plugin_service::ListAndWatchMessageKind>,
        >,
        connectivity_status: InstanceConnectivityStatus,
        config_name: &str,
    ) -> InstanceMap {
        Arc::new(tokio::sync::Mutex::new(
            discovery_results
                .iter()
                .map(|device| {
                    let (list_and_watch_message_sender, list_and_watch_message_receiver) =
                        broadcast::channel(2);
                    list_and_watch_message_receivers.push(list_and_watch_message_receiver);
                    let instance_name = get_device_instance_name(&device.id, &config_name);
                    (
                        instance_name,
                        InstanceInfo {
                            list_and_watch_message_sender,
                            connectivity_status: connectivity_status.clone(),
                        },
                    )
                })
                .collect(),
        ))
    }

    fn create_mock_discovery_operator(
        discovery_handler_map: RegisteredDiscoveryHandlerMap,
        config: KubeAkriConfig,
        instance_map: InstanceMap,
        device_plugin_path: &str,
        // stream_type: Option<StreamType>
    ) -> MockDiscoveryOperator {
        let ctx = MockDiscoveryOperator::new_context();
        let discovery_handler_map_clone = discovery_handler_map.clone();
        let config_clone = config.clone();
        let instance_map_clone = instance_map.clone();
        let device_plugin_path_clone = device_plugin_path.to_string();
        ctx.expect().return_once(move |_, _, _, _| {
            // let mut discovery_handler_status_seq = Sequence::new();
            let mut mock = MockDiscoveryOperator::default();
            mock.expect_get_discovery_handler_map()
                .return_once(move || discovery_handler_map_clone);
            mock.expect_get_config().return_once(move || config_clone);
            mock.expect_get_instance_map()
                .return_once(move || instance_map_clone);
            mock.expect_get_device_plugin_path()
                .return_once(move || device_plugin_path_clone);
            mock
        });
        let mock = MockDiscoveryOperator::new(
            discovery_handler_map,
            config,
            instance_map,
            device_plugin_path.to_string(),
        );
        mock
    }

    // Creates a RegisteredDiscoveryHandlerMap and adds an entry for a debugEcho discovery handler
    fn create_discovery_handler_map(
        protocol_name: &str,
        endpoint: &str,
    ) -> RegisteredDiscoveryHandlerMap {
        let discovery_handler_details = create_discovery_handler_details(protocol_name, endpoint);
        // Add discovery handler to registered discovery handler map
        let mut protocol_dh_map = HashMap::new();
        protocol_dh_map.insert(endpoint.to_string(), discovery_handler_details);
        let mut inner_dh_map = HashMap::new();
        inner_dh_map.insert(protocol_name.to_string(), protocol_dh_map);
        Arc::new(std::sync::Mutex::new(inner_dh_map))
    }

    fn create_discovery_handler_details(
        protocol_name: &str,
        endpoint: &str,
    ) -> DiscoveryHandlerDetails {
        let register_request = RegisterRequest {
            protocol: protocol_name.to_string(),
            endpoint: endpoint.to_string(),
            is_local: true,
        };
        let (stop_discovery, _) = broadcast::channel(2);
        DiscoveryHandlerDetails {
            register_request,
            stop_discovery: stop_discovery.clone(),
            connectivity_status: DiscoveryHandlerConnectivityStatus::Online,
        }
    }

    fn setup_test_do_discover() -> MockDiscoveryOperator {
        let discovery_handler_map = create_discovery_handler_map("debugEcho", "socket.sock");

        // Build discovery operator
        let path_to_config = "../test/yaml/config-a.yaml";
        let config_yaml = std::fs::read_to_string(path_to_config).expect("Unable to read file");
        let config: KubeAkriConfig = serde_yaml::from_str(&config_yaml).unwrap();
        create_mock_discovery_operator(
            discovery_handler_map,
            config,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "random",
        )
    }

    #[test]
    fn test_generate_instance_digest() {
        let mut mock_env_var_a = MockEnvVarQuery::new();
        mock_env_var_a
            .expect_get_env_var()
            .returning(|_| Ok("node-a".to_string()));
        let id = "video1";
        let first_unshared_video_digest =
            inner_generate_instance_digest(id, false, &mock_env_var_a);
        let first_shared_video_digest = inner_generate_instance_digest(id, true, &mock_env_var_a);
        let mut mock_env_var_b = MockEnvVarQuery::new();
        mock_env_var_b
            .expect_get_env_var()
            .returning(|_| Ok("node-b".to_string()));
        let second_unshared_video_digest =
            inner_generate_instance_digest(id, false, &mock_env_var_b);
        let second_shared_video_digest = inner_generate_instance_digest(id, true, &mock_env_var_b);
        // unshared instances visible to different nodes should NOT have the same digest
        assert_ne!(first_unshared_video_digest, second_unshared_video_digest);
        // shared instances visible to different nodes should have the same digest
        assert_eq!(first_shared_video_digest, second_shared_video_digest);
    }

    // Test that DH is connected to on second try getting stream and
    // that connectivity status is changed from Online -> HasClient -> Online again
    // when a successful connection is made and completed.
    #[tokio::test]
    async fn test_do_discover_completed_internal_connection() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_discovery_operator = setup_test_do_discover();
        let mut get_stream_seq = Sequence::new();
        // First time cannot get stream and is marked offline
        mock_discovery_operator
            .expect_get_stream()
            .times(1)
            .returning(|_, _| None)
            .in_sequence(&mut get_stream_seq);
        mock_discovery_operator
            .expect_mark_offline_or_deregister_discovery_handler()
            .withf(move |endpoint: &str| endpoint == "socket.sock")
            .times(1)
            .returning(|_| Ok(false));
        // Second time successfully get stream
        let (_, rx) = mpsc::channel(2);
        let stream_type = Some(StreamType::Internal(rx));
        mock_discovery_operator
            .expect_get_stream()
            .times(1)
            .return_once(move |_, _| stream_type)
            .in_sequence(&mut get_stream_seq);
        // Make sure discovery handler is marked as HasClient
        let mut discovery_handler_status_seq = Sequence::new();
        mock_discovery_operator
            .expect_set_discovery_handler_connectivity_status()
            .withf(
                move |endpoint: &str, connectivity_status: &DiscoveryHandlerConnectivityStatus| {
                    endpoint == "socket.sock"
                        && connectivity_status == &DiscoveryHandlerConnectivityStatus::HasClient
                },
            )
            .times(1)
            .returning(|_, _| ())
            .in_sequence(&mut discovery_handler_status_seq);
        // Discovery should be initiated
        mock_discovery_operator
            .expect_internal_do_discover()
            .times(1)
            .returning(|_, _, _, _| Ok(()));
        // Make sure after discovery is complete that the DH is marked Online again
        mock_discovery_operator
            .expect_set_discovery_handler_connectivity_status()
            .withf(
                move |endpoint: &str, connectivity_status: &DiscoveryHandlerConnectivityStatus| {
                    endpoint == "socket.sock"
                        && connectivity_status == &DiscoveryHandlerConnectivityStatus::Online
                },
            )
            .times(1)
            .returning(|_, _| ())
            .in_sequence(&mut discovery_handler_status_seq);
        let mock_kube_interface: Arc<Box<dyn k8s::KubeInterface>> =
            Arc::new(Box::new(MockKubeInterface::new()));
        start_discovery::do_discover(Arc::new(mock_discovery_operator), mock_kube_interface)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_handle_discovery_results() {
        let _ = env_logger::builder().is_test(true).try_init();
        // Set node name for generating instance id
        std::env::set_var("AGENT_NODE_NAME", "node-a");
        let mock_kube_interface: Arc<Box<dyn k8s::KubeInterface>> =
            Arc::new(Box::new(MockKubeInterface::new()));
        let discovery_handler_map: RegisteredDiscoveryHandlerMap =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let path_to_config = "../test/yaml/config-a.yaml";
        let config_yaml = std::fs::read_to_string(path_to_config).expect("Unable to read file");
        let config: KubeAkriConfig = serde_yaml::from_str(&config_yaml).unwrap();
        let config_name = config.metadata.name.clone();
        INSTANCE_COUNT_METRIC
            .with_label_values(&[&config_name, "true"])
            .set(0);
        let device1 = Device {
            id: "device1".to_string(),
            properties: HashMap::new(),
            mounts: Vec::default(),
            device_specs: Vec::default(),
        };
        let device2 = Device {
            id: "device2".to_string(),
            properties: HashMap::new(),
            mounts: Vec::default(),
            device_specs: Vec::default(),
        };
        let discovery_results: Vec<Device> = vec![device1, device2];
        let discovery_operator = Arc::new(DiscoveryOperator::new(
            discovery_handler_map,
            config,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "random_path".to_string(),
        ));
        let mut mock_device_plugin_builder = MockDevicePluginBuilderInterface::new();
        mock_device_plugin_builder
            .expect_build_device_plugin()
            .times(2)
            .returning(move |_, _, _, _, _, _, _, _, _| Ok(()));
        discovery_operator
            .handle_discovery_results(
                mock_kube_interface,
                discovery_results,
                true,
                Box::new(mock_device_plugin_builder),
            )
            .await
            .unwrap();

        assert_eq!(
            INSTANCE_COUNT_METRIC
                .with_label_values(&[&config_name, "true"])
                .get(),
            2
        );
    }

    // 1: InstanceConnectivityStatus of all instances that go offline is changed from Online to Offline
    // 2: InstanceConnectivityStatus of shared instances that come back online in under 5 minutes is changed from Offline to Online
    // 3: InstanceConnectivityStatus of unshared instances that come back online before next periodic discovery is changed from Offline to Online
    #[tokio::test(core_threads = 2)]
    async fn test_update_connectivity_status_factory() {
        let _ = env_logger::builder().is_test(true).try_init();
        let path_to_config = "../test/yaml/config-a.yaml";
        let config_yaml = std::fs::read_to_string(path_to_config).expect("Unable to read file");
        let config: KubeAkriConfig = serde_yaml::from_str(&config_yaml).unwrap();
        let config_name = config.metadata.name.clone();
        let mut list_and_watch_message_receivers = Vec::new();
        let mut visible_discovery_results = Vec::new();
        let discovery_handler_map: RegisteredDiscoveryHandlerMap =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let discovery_handler_map_clone = discovery_handler_map.clone();
        register_embedded_discovery_handlers(discovery_handler_map_clone).unwrap();

        //
        // 1: Assert that InstanceConnectivityStatus of non local instances that are no longer visible is changed to Offline
        //
        let instance_map: InstanceMap = build_instance_map(
            &config,
            &mut visible_discovery_results,
            &mut list_and_watch_message_receivers,
            InstanceConnectivityStatus::Online,
        )
        .await;
        let is_local = false;
        run_update_connectivity_status(
            config.clone(),
            HashMap::new(),
            is_local,
            instance_map.clone(),
            discovery_handler_map.clone(),
            MockKubeInterface::new(),
        )
        .await;
        // Make sure update_connectivity_status has updated the map before grabbing it
        tokio::time::delay_for(Duration::from_millis(500)).await;
        let unwrapped_instance_map = instance_map.lock().await.clone();
        for (_, instance_info) in unwrapped_instance_map {
            assert_ne!(
                instance_info.connectivity_status,
                InstanceConnectivityStatus::Online
            );
        }

        //
        // 2: Assert that InstanceConnectivityStatus of non local instances that come back online in <5 mins is changed to Online
        //
        let instance_map: InstanceMap = build_instance_map(
            &config,
            &mut visible_discovery_results,
            &mut list_and_watch_message_receivers,
            InstanceConnectivityStatus::Offline(Instant::now()),
        )
        .await;
        let currently_visible_instances: HashMap<String, Device> = visible_discovery_results
            .iter()
            .map(|device| {
                let instance_name = get_device_instance_name(&device.id, &config_name);
                (instance_name, device.clone())
            })
            .collect();
        let is_local = false;
        run_update_connectivity_status(
            config.clone(),
            currently_visible_instances.clone(),
            is_local,
            instance_map.clone(),
            discovery_handler_map.clone(),
            MockKubeInterface::new(),
        )
        .await;
        // Make sure update_connectivity_status has updated the map before grabbing it
        tokio::time::delay_for(Duration::from_millis(500)).await;
        let unwrapped_instance_map = instance_map.lock().await.clone();
        for (_, instance_info) in unwrapped_instance_map {
            assert_eq!(
                instance_info.connectivity_status,
                InstanceConnectivityStatus::Online
            );
        }

        //
        // 4: Assert that local devices that go offline are removed from the instance map
        //
        let mut mock = MockKubeInterface::new();
        mock.expect_delete_instance()
            .times(2)
            .returning(move |_, _| Ok(()));

        let instance_map: InstanceMap = build_instance_map(
            &config,
            &mut visible_discovery_results,
            &mut list_and_watch_message_receivers,
            InstanceConnectivityStatus::Online,
        )
        .await;
        let is_local = true;
        run_update_connectivity_status(
            config,
            HashMap::new(),
            is_local,
            instance_map.clone(),
            discovery_handler_map.clone(),
            mock,
        )
        .await;
        // Make sure update_connectivity_status has updated the map before grabbing it
        tokio::time::delay_for(Duration::from_millis(500)).await;
        let unwrapped_instance_map = instance_map.lock().await.clone();
        assert!(unwrapped_instance_map.is_empty());
    }

    async fn run_update_connectivity_status(
        config: KubeAkriConfig,
        currently_visible_instances: HashMap<String, Device>,
        is_local: bool,
        instance_map: InstanceMap,
        discovery_handler_map: RegisteredDiscoveryHandlerMap,
        mock: MockKubeInterface,
    ) {
        let device_plugin_temp_dir = Builder::new().prefix("device-plugins-").tempdir().unwrap();
        let device_plugin_temp_dir_path = device_plugin_temp_dir.path().to_str().unwrap();
        let discovery_operator = Arc::new(DiscoveryOperator::new(
            discovery_handler_map,
            config,
            instance_map.clone(),
            device_plugin_temp_dir_path.to_string(),
        ));
        discovery_operator
            .update_connectivity_status(
                Arc::new(Box::new(mock)),
                currently_visible_instances,
                is_local,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_discovery_handler_connectivity_status() {
        let _ = env_logger::builder().is_test(true).try_init();
        let path_to_config = "../test/yaml/config-a.yaml";
        let config_yaml = std::fs::read_to_string(path_to_config).expect("Unable to read file");
        let config: KubeAkriConfig = serde_yaml::from_str(&config_yaml).unwrap();
        let protocol = "debugEcho";
        let endpoint = "socket.sock";
        let discovery_handler_map = create_discovery_handler_map(protocol, endpoint);
        let discovery_operator = DiscoveryOperator::new(
            discovery_handler_map,
            config,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "random".to_string(),
        );
        // Test that an online discovery handler is marked HasClient
        discovery_operator.set_discovery_handler_connectivity_status(
            endpoint,
            DiscoveryHandlerConnectivityStatus::HasClient,
        );
        assert_eq!(
            discovery_operator
                .discovery_handler_map
                .lock()
                .unwrap()
                .get_mut(protocol)
                .unwrap()
                .clone()
                .get(endpoint)
                .unwrap()
                .clone()
                .connectivity_status,
            DiscoveryHandlerConnectivityStatus::HasClient
        );
        // Test that no change happens when random endpoint passed
        discovery_operator.set_discovery_handler_connectivity_status(
            "random",
            DiscoveryHandlerConnectivityStatus::Online,
        );
        assert_eq!(
            discovery_operator
                .discovery_handler_map
                .lock()
                .unwrap()
                .get_mut(protocol)
                .unwrap()
                .clone()
                .get(endpoint)
                .unwrap()
                .clone()
                .connectivity_status,
            DiscoveryHandlerConnectivityStatus::HasClient
        );
    }

    #[tokio::test]
    async fn test_mark_offline_or_deregister_discovery_handler() {
        let _ = env_logger::builder().is_test(true).try_init();
        let path_to_config = "../test/yaml/config-a.yaml";
        let config_yaml = std::fs::read_to_string(path_to_config).expect("Unable to read file");
        let config: KubeAkriConfig = serde_yaml::from_str(&config_yaml).unwrap();
        let protocol = "debugEcho";
        let endpoint = "socket.sock";
        let discovery_handler_map = create_discovery_handler_map(protocol, endpoint);
        let discovery_operator = DiscoveryOperator::new(
            discovery_handler_map,
            config,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "random".to_string(),
        );
        // Test that an online discovery handler is marked offline
        assert_eq!(
            discovery_operator
                .mark_offline_or_deregister_discovery_handler(endpoint)
                .await
                .unwrap(),
            false
        );
        if let DiscoveryHandlerConnectivityStatus::Offline(_) = discovery_operator
            .discovery_handler_map
            .lock()
            .unwrap()
            .get_mut(protocol)
            .unwrap()
            .clone()
            .get(endpoint)
            .unwrap()
            .clone()
            .connectivity_status
        {
            // expected
        } else {
            panic!("DiscoveryHandlerConnectivityStatus should be changed to offline");
        }
        // Test that an offline discovery handler is not deregistered if the time has not passed
        assert_eq!(
            discovery_operator
                .mark_offline_or_deregister_discovery_handler(endpoint)
                .await
                .unwrap(),
            false
        );
    }

    #[tokio::test]
    async fn test_get_stream_embedded() {
        let _ = env_logger::builder().is_test(true).try_init();
        std::env::set_var("ENABLE_DEBUG_ECHO", "yes");
        let path_to_config = "../test/yaml/config-a.yaml";
        let config_yaml = std::fs::read_to_string(path_to_config).expect("Unable to read file");
        let config: KubeAkriConfig = serde_yaml::from_str(&config_yaml).unwrap();
        let discovery_handler_map = Arc::new(std::sync::Mutex::new(HashMap::new()));
        // "register" debug echo discovery handler by adding it to the registered DH map
        let debug_echo_reg_req =
            akri_debug_echo::get_register_request(EMBEDDED_DISCOVERY_HANDLER_ENDPOINT);
        let (tx, _) = broadcast::channel(2);
        let discovery_handler_details = DiscoveryHandlerDetails {
            register_request: debug_echo_reg_req.clone(),
            stop_discovery: tx,
            connectivity_status: DiscoveryHandlerConnectivityStatus::Online,
        };
        let mut register_request_map = HashMap::new();
        register_request_map.insert(debug_echo_reg_req.endpoint, discovery_handler_details);
        discovery_handler_map
            .lock()
            .unwrap()
            .insert(debug_echo_reg_req.protocol, register_request_map);
        let discovery_operator = DiscoveryOperator::new(
            discovery_handler_map,
            config,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "random".to_string(),
        );
        // test embedded debugEcho socket
        if let Some(StreamType::Internal(_)) =
            discovery_operator.get_stream("embedded", "random").await
        {
            // expected
        } else {
            panic!("expected internal stream");
        }
    }

    #[tokio::test]
    async fn test_get_stream_external() {
        use akri_discovery_utils::discovery::mock_discovery_handler;
        let _ = env_logger::builder().is_test(true).try_init();
        let path_to_config = "../test/yaml/config-a.yaml";
        let config_yaml = std::fs::read_to_string(path_to_config).expect("Unable to read file");
        let config: KubeAkriConfig = serde_yaml::from_str(&config_yaml).unwrap();
        let protocol = "mock";
        let (mock_dh_dir, endpoint) =
            mock_discovery_handler::get_mock_discovery_handler_dir_and_endpoint("mock.sock");
        let discovery_handler_map = Arc::new(std::sync::Mutex::new(HashMap::new()));
        // "register" debug echo discovery handler by adding it to the registered DH map
        let register_request = RegisterRequest {
            protocol: protocol.to_string(),
            endpoint: endpoint.clone(),
            is_local: true,
        };
        let (tx, _) = broadcast::channel(2);
        let discovery_handler_details = DiscoveryHandlerDetails {
            register_request,
            stop_discovery: tx,
            connectivity_status: DiscoveryHandlerConnectivityStatus::Online,
        };
        let mut register_request_map = HashMap::new();
        register_request_map.insert(endpoint.clone(), discovery_handler_details);
        discovery_handler_map
            .lock()
            .unwrap()
            .insert(protocol.to_string(), register_request_map);
        let discovery_operator = DiscoveryOperator::new(
            discovery_handler_map,
            config,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "random".to_string(),
        );
        // Should not be able to get stream if DH is not running
        assert!(discovery_operator
            .get_stream(&endpoint, &mock_dh_dir)
            .await
            .is_none());
        // Start mock DH
        let _dh_server_thread_handle =
            mock_discovery_handler::run_mock_discovery_handler(&mock_dh_dir, &endpoint).await;
        if let Some(StreamType::External(_)) =
            discovery_operator.get_stream(&endpoint, &mock_dh_dir).await
        {
            // expected
        } else {
            panic!("expected external stream");
        }
    }
}
