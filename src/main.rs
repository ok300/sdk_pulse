use std::fs::OpenOptions;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use bip39::{Language, Mnemonic};
use breez_sdk_core::*;
use figment::providers::{Format, Toml};
use figment::Figment;
use log::info;
use serde::Deserialize;

struct AppEventListener {}
impl EventListener for AppEventListener {
    fn on_event(&self, e: BreezEvent) {
        info!("Received Breez event: {e:?}");
    }
}

/// On first run (if you don't already have a node), set the `invite_code` and leave `mnemonic` as None.
///
/// On subsequent runs, or if you already have a node, set the `mnemonic`. The `invite_code` can be left empty.
async fn get_sdk(
    breez_sdk_api_key: &str,
    working_dir: &str,
    invite_code: Option<&str>,
    mnemonic: Option<&str>,
) -> Result<Arc<BreezServices>> {
    let mnemonic_obj = match mnemonic {
        None => {
            let mnemonic = Mnemonic::generate_in(Language::English, 12)?;
            println!("Generated mnemonic: {mnemonic}");
            mnemonic
        }
        Some(mnemonic_str) => Mnemonic::from_str(mnemonic_str)?,
    };

    let seed = mnemonic_obj.to_seed("");

    let mut config = BreezServices::default_config(
        EnvironmentType::Production,
        breez_sdk_api_key.into(),
        breez_sdk_core::NodeConfig::Greenlight {
            config: GreenlightNodeConfig {
                partner_credentials: None,
                invite_code: invite_code.map(Into::into),
            },
        },
    );
    config.working_dir = working_dir.into();

    // Create working dir if it doesn't exist
    std::fs::create_dir_all(working_dir)?;

    let sdk = BreezServices::connect(config, seed.to_vec(), Box::new(AppEventListener {})).await?;

    Ok(sdk)
}

#[derive(Debug, PartialEq, Deserialize)]
struct PulseConfig {
    breez_api_key: String,
    sdk_1_mnemonic: String,
    sdk_2_mnemonic: String,

    /// Relative or absolute path to the CSV file with iteration measurements
    iterations_csv_full_path: String,
    /// Relative or absolute to where the iteration logs folders will be placed
    iterations_logs_dir_path: String,

    ln_address_wos: String,
    ln_address_tor_node: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let start = SystemTime::now();
    let start_ts = start.duration_since(UNIX_EPOCH)?.as_secs();

    let figment = Figment::new().merge(Toml::file("pulse-config.toml"));
    let config: PulseConfig = figment.extract()?;

    let log_dir = &format!("{}/sdk-log-{start_ts}", config.iterations_logs_dir_path);
    std::fs::create_dir_all(log_dir)?;
    BreezServices::init_logging(log_dir, None)?;

    let sdk_1 = get_sdk(
        &config.breez_api_key,
        "working-dir-sdk-1",
        None,
        Some(&config.sdk_1_mnemonic),
    )
    .await?;
    info!("[sdk_1] Node info: {:?}", sdk_1.node_info()?);

    let sdk_2 = get_sdk(
        &config.breez_api_key,
        "working-dir-sdk-2",
        None,
        Some(&config.sdk_2_mnemonic),
    )
    .await?;
    info!("[sdk_2] Node info: {:?}", sdk_2.node_info()?);

    // Make sure we finish a full sync, e.g. no background sync is running from this point on
    // We want to avoid any background sync threads affecting the next measurements
    sdk_1.sync().await?;
    sdk_2.sync().await?;

    info!("Testing GL-2-GL");
    let gl2gl_res = pay_gl_2_gl(sdk_1.clone(), sdk_2.clone()).await;
    info!("Testing GL-2-WoS");
    let gl2wos_res = pay_gl_2_ln_address(sdk_1.clone(), &config.ln_address_wos).await;
    info!("Testing GL-2-Tor");
    let gl2tor_res = pay_gl_2_ln_address(sdk_1.clone(), &config.ln_address_tor_node).await;

    sdk_1.disconnect().await?;
    sdk_2.disconnect().await?;

    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(config.iterations_csv_full_path)?;
    let mut wtr = csv::Writer::from_writer(file);
    wtr.write_record(&[
        start_ts.to_string(),
        gl2wos_res.0.map(|d| d.to_string()).unwrap_or_default(),
        gl2wos_res.1,
        gl2gl_res.0.map(|d| d.to_string()).unwrap_or_default(),
        gl2gl_res.1,
        gl2tor_res.0.map(|d| d.to_string()).unwrap_or_default(),
        gl2tor_res.1,
    ])?;
    wtr.flush()?;

    Ok(())
}

async fn pay_gl_2_ln_address(
    sdk_sender: Arc<BreezServices>,
    ln_address: &str,
) -> (Option<u64>, String) {
    let ts_start = Instant::now();

    match parse(ln_address).await {
        Ok(InputType::LnUrlPay { data }) => {
            match sdk_sender
                .lnurl_pay(1, Some("test-gl2lnurl".into()), data)
                .await
            {
                Ok(_) => (
                    Some(Instant::now().duration_since(ts_start).as_secs()),
                    "Ok".into(),
                ),
                Err(e) => (None, e.to_string()),
            }
        }
        _ => (None, "Failed to parse LN Address".into()),
    }
}

async fn pay_gl_2_gl(
    sdk_sender: Arc<BreezServices>,
    sdk_receiver: Arc<BreezServices>,
) -> (Option<u64>, String) {
    info!("[sdk-rx] Creating invoice");
    match sdk_receiver
        .receive_payment(ReceivePaymentRequest {
            amount_sats: 1,
            description: "test-gl2gl".to_string(),
            preimage: None,
            opening_fee_params: None,
            use_description_hash: None,
            expiry: None,
            cltv: None,
        })
        .await
    {
        Ok(recv_payment) => {
            let ts_start = Instant::now();

            info!("[sdk-tx] Paying invoice");
            match sdk_sender
                .send_payment(recv_payment.ln_invoice.bolt11, None)
                .await
            {
                Ok(_) => (
                    Some(Instant::now().duration_since(ts_start).as_secs()),
                    "Ok".into(),
                ),
                Err(e) => (None, format!("[sdk-tx] Failed to send payment: {e}")),
            }
        }
        Err(e) => (None, format!("[sdk-rx] Failed to create invoice: {e}")),
    }
}
