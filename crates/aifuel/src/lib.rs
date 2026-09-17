pub mod launcher;

use aifuel_app::MonitoringFacade;
use aifuel_providers::{CollectionConfig, DiscoveryContext, ProviderMonitoring};

/// Construct the monitoring dependencies at the executable boundary.
pub fn monitoring_facade() -> Result<MonitoringFacade<ProviderMonitoring>, String> {
    let context = DiscoveryContext::from_environment().map_err(|error| error.to_string())?;
    let monitoring =
        ProviderMonitoring::new(context.home_dir(), CollectionConfig::from_environment())?;
    Ok(MonitoringFacade::new(monitoring))
}
