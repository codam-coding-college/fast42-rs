use config;
use serde::Deserialize;
use secrecy::{ExposeSecret, Secret};

#[derive(Deserialize)]
pub struct Settings {
    pub fortytwo: FortyTwoAPICredentials,
}

#[derive(Deserialize)]
pub struct FortyTwoAPICredentials {
    pub uid: String,
    pub secret: Secret<String>,
}

pub fn get_configuration_settings() -> Result<Settings, config::ConfigError> {
    let base_path = std::env::current_dir().expect("Failed to determine the current directory");
    let secrets_filename = "secrets.yaml";
    let settings = config::Config::builder()
        .add_source(config::File::from(base_path.join(&secrets_filename)))
        .build()?;
    settings.try_deserialize::<Settings>()
}
