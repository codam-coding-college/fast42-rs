use fast42::{HttpOptions, Fast42};
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
        .add_source(config::File::from(base_path.join("examples").join(&secrets_filename)))
        .build()?;
    settings.try_deserialize::<Settings>()
}

#[tokio::main]
async fn main() {
    let credentials = get_configuration_settings().unwrap().fortytwo;
    let fast42 = Fast42::new(
        &credentials.uid,
        &credentials.secret.expose_secret(),
        14000,
        8,
    );
    let result = fast42
        .get_all_async_pages(
            "/users".to_string(),
            vec![HttpOptions::new("filter[primary_campus_id]", "14")],
        )
        .await;
    match result {
        Ok(mut pages) => {
            let last_index = pages.len();
            let last_page = pages.remove(last_index - 1).text().await.unwrap();
            assert_ne!(last_page, "");
            println!("Succesfully fetched {} user pages.", last_index);
        }
        Err(e) => {
            println!("{}", e);
            assert!(false);
        }
    }
}
