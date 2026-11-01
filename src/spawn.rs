//! Relay spawn utilities.
use crate::{
    asset::AssetInfoService,
    auth::{DevBypass, JwksCache, JwtAuthLayer},
    chains::Chains,
    cli::Args,
    config::{RelayConfig, WellKnownConfig},
    constants::{DEFAULT_PORTO_BASE_URL, ESCROW_REFUND_DURATION_SECS},
    diagnostics::run_diagnostics,
    metrics::{self, HttpTracingService, RpcMetricsService},
    price::{PriceFetcher, PriceOracle, PriceOracleConfig},
    rpc::{AccountApiServer, AccountRpc, Relay, RelayApiServer},
    signers::DynSigner,
    storage::RelayStorage,
    twilio::TwilioClient,
    types::VersionedContracts,
    version::RELAY_LONG_VERSION,
};
use ::metrics::counter;
use alloy::{primitives::B256, signers::local::LocalSigner};
use eyre::WrapErr;
use http::header;
use itertools::Itertools;
use jsonrpsee::{
    RpcModule,
    server::{
        Server, ServerConfig, ServerHandle,
        middleware::{http::ProxyGetRequestLayer, rpc::RpcServiceBuilder},
    },
    types::ErrorObjectOwned,
};
use metrics_exporter_prometheus::PrometheusHandle;
use resend_rs::Resend;
use sqlx::PgPool;
use std::{net::SocketAddr, path::Path, sync::Arc};
use tower::ServiceBuilder;
use tower_http::cors::{AllowMethods, AllowOrigin, CorsLayer};
use tracing::{info, warn};

/// Context returned once relay is launched.
#[derive(Debug, Clone)]
pub struct RelayHandle {
    /// The socket address to which the server is bound.
    pub local_addr: SocketAddr,
    /// Handle to RPC server.
    pub server: ServerHandle,
    /// Configured providers.
    pub chains: Arc<Chains>,
    /// Storage of the relay.
    pub storage: RelayStorage,
    /// Metrics collector handle.
    pub metrics: PrometheusHandle,
    /// Price oracle.
    pub price_oracle: PriceOracle,
}

impl RelayHandle {
    /// Returns the url to the http server
    pub fn http_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }
}

/// Attempts to spawn the relay service using CLI arguments and a configuration file.
pub async fn try_spawn_with_args(args: Args, config_path: &Path) -> eyre::Result<RelayHandle> {
    let skip_diagnostics = args.skip_diagnostics;
    let config = if !config_path.exists() {
        let config = args.merge_relay_config(RelayConfig::default());
        config.save_to_file(config_path)?;
        config
    } else if !args.config_only {
        // File exists: load and override with CLI values.
        args.merge_relay_config(RelayConfig::load_from_file(config_path)?)
    } else {
        let mut config = RelayConfig::load_from_file(config_path)?;
        config.secrets.signers_mnemonic = std::env::var("RELAY_MNEMONIC")
            .wrap_err("Missing environment variable RELAY_MNEMONIC")?
            .parse()
            .wrap_err("Invalid value for RELAY_MNEMONIC")?;
        config.secrets.funder_key = std::env::var("RELAY_FUNDER_SIGNER_KEY")
            .wrap_err("Missing environment variable RELAY_FUNDER_SIGNER_KEY")?;
        config.database_url = std::env::var("RELAY_DB_URL").ok();

        // LOCAL-ONLY dev sponsorship escape hatch, opt-in via env. Requires an
        // `[auth]` block. A boot assertion (`assert_dev_hatch_safe`) refuses to
        // start if this ends up set on a mainnet relay.
        if let Ok(dev_key) = std::env::var("RELAY_DEV_API_KEY") {
            match config.auth.as_mut() {
                Some(auth) => auth.dev_api_key = Some(dev_key),
                None => warn!(
                    "RELAY_DEV_API_KEY is set but no [auth] config is present; dev escape hatch ignored"
                ),
            }
        }

        if let (Ok(account_sid), Ok(auth_token), Ok(verify_service_sid)) = (
            std::env::var("TWILIO_ACCOUNT_SID"),
            std::env::var("TWILIO_AUTH_TOKEN"),
            std::env::var("TWILIO_VERIFY_SERVICE_SID"),
        ) {
            config = config.with_twilio_credentials(account_sid, auth_token, verify_service_sid);
        }

        config
            .with_resend_api_key(std::env::var("RESEND_API_KEY").ok())
            .with_onramp_worker_secret(std::env::var("ONRAMP_WORKER_SECRET").ok())
            .with_simple_settler_owner_key(std::env::var("RELAY_SETTLER_OWNER_KEY").ok())
            .with_funder_owner_key(std::env::var("RELAY_FUNDER_OWNER_KEY").ok())
            .with_binance_keys(
                std::env::var("BINANCE_API_KEY").ok(),
                std::env::var("BINANCE_API_SECRET").ok(),
            )
    };

    try_spawn(config, skip_diagnostics).await
}

/// Spawns the relay service using the provided [`RelayConfig`].
pub async fn try_spawn(config: RelayConfig, skip_diagnostics: bool) -> eyre::Result<RelayHandle> {
    // Fail-closed guard: the local dev sponsorship escape hatch must never run
    // on a relay that serves a mainnet chain. Checked before anything else.
    assert_dev_hatch_safe(&config)?;

    // construct db
    let storage = if let Some(db_url) = &config.database_url {
        info!("Using PostgreSQL as storage.");
        let pool = PgPool::connect(db_url).await?;
        sqlx::migrate!().run(&pool).await?;

        RelayStorage::pg(pool)
    } else {
        info!("Using in-memory storage.");
        RelayStorage::in_memory()
    };

    // setup metrics exporter and periodic metric collectors
    let metrics =
        metrics::setup_exporter((config.server.address, config.server.metrics_port)).await;

    // derive signers, as many as we need for the chain with the most signers
    let signers = DynSigner::derive_from_mnemonic(
        config.secrets.signers_mnemonic.clone(),
        config.max_signer_count(),
    )?;
    let signer_addresses = signers.iter().map(|signer| signer.address()).collect::<Vec<_>>();
    info!("Using [{}] signers: {signer_addresses:?}", config.max_signer_count());

    // setup funder signer
    let funder_signer = DynSigner::from_raw(&config.secrets.funder_key).await?;

    // construct asset info service
    let asset_info = AssetInfoService::new(512, &config);
    let asset_info_handle = asset_info.handle();
    tokio::spawn(asset_info);

    // build chains
    let chains =
        Arc::new(Chains::new(signers, storage.clone(), &config, asset_info_handle.clone()).await?);

    // Run pre-flight diagnostics
    if skip_diagnostics {
        warn!("Skipping pre-flight diagnostics.");
    } else {
        info!("Running pre-flight diagnostics.");
        let report = run_diagnostics(&config, chains.clone()).await?;
        report.log();

        if report.has_errors() {
            eyre::bail!(
                "Pre-flight diagnostics failed with errors. Please fix the issues before starting the relay."
            );
        }
    }

    metrics::spawn_periodic_collectors(&config, storage.clone(), chains.clone()).await?;

    // construct quote signer
    let quote_signer = DynSigner(Arc::new(LocalSigner::from_bytes(&B256::random())?));
    let quote_signer_addr = quote_signer.address();

    // construct rpc module
    let mut price_oracle = PriceOracle::new(PriceOracleConfig { rate_ttl: config.quote.rate_ttl })
        .with_storage(storage.clone());
    if let Some(constant_rate) = config.quote.constant_rate {
        warn!("Setting a constant price rate: {constant_rate}. Should not be used in production!");
        price_oracle = price_oracle.with_constant_rate(constant_rate);
    } else {
        price_oracle.spawn_fetcher(PriceFetcher::CoinGecko, &config);
    }

    // get contract versions from chain.
    let contracts = VersionedContracts::new(
        &config,
        chains
            .chains_iter()
            .next()
            .map(|chain| chain.provider())
            .expect("should have at least one"),
    )
    .await?;

    let relay = Relay::new(
        contracts,
        chains.clone(),
        quote_signer,
        funder_signer.clone(),
        config.quote,
        price_oracle.clone(),
        config.fee_recipient,
        storage.clone(),
        asset_info_handle,
        config
            .interop
            .as_ref()
            .map(|i| i.escrow_refund_threshold)
            .unwrap_or(ESCROW_REFUND_DURATION_SECS),
        config.sponsorship,
        config.chain_sponsorship,
        config.fee_payers,
    );
    // Setup account RPC module if email and onramp worker secret are configured
    let account_rpc = if let (Some(resend_api_key), Some(onramp_worker_secret)) =
        (&config.email.resend_api_key, &config.secrets.onramp_worker_secret)
    {
        // Check if phone verification is also configured
        let rpc = if let Some(phone_config) = &config.phone {
            let twilio_client = TwilioClient::new(
                phone_config.twilio_account_sid.clone(),
                phone_config.twilio_auth_token.clone(),
                phone_config.twilio_verify_service_sid.clone(),
            );

            AccountRpc::with_phone(
                relay.clone(),
                Resend::new(resend_api_key),
                storage.clone(),
                config.email.porto_base_url.clone().unwrap_or(DEFAULT_PORTO_BASE_URL.to_string()),
                twilio_client,
                phone_config.clone(),
                onramp_worker_secret.clone(),
            )
        } else {
            AccountRpc::new(
                relay.clone(),
                Resend::new(resend_api_key),
                storage.clone(),
                config.email.porto_base_url.clone().unwrap_or(DEFAULT_PORTO_BASE_URL.to_string()),
                onramp_worker_secret.clone(),
            )
        };

        Some(rpc.into_rpc())
    } else {
        None
    };
    let mut rpc = relay.into_rpc();
    let well_known_routes = register_well_known(&mut rpc, &config.well_known)?;

    // http layers
    let cors = CorsLayer::new()
        .allow_methods(AllowMethods::any())
        .allow_origin(AllowOrigin::any())
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    // Resolves Better Auth bearer JWTs (when `auth.jwks_url` is configured) and
    // threads the verified user id into request extensions for user-tied
    // sponsorship quota. A configured `auth.dev_api_key` additionally enables the
    // LOCAL-ONLY static escape hatch (already asserted mainnet-safe at boot). No
    // `auth` config = pass-through (address-mode quota).
    let jwt_auth = {
        let cache = config.auth.as_ref().map(|auth| JwksCache::new(auth.jwks_url.clone()));
        let dev = config.auth.as_ref().and_then(|auth| {
            auth.dev_api_key.as_ref().map(|key| DevBypass {
                api_key: key.clone(),
                subject: auth.dev_subject.clone().unwrap_or_else(|| "dev-local".to_string()),
            })
        });
        JwtAuthLayer::new(cache, dev)
    };

    // start server
    let server = Server::builder()
        .set_config(
            ServerConfig::builder()
                .http_only()
                .max_connections(config.server.max_connections)
                .build(),
        )
        .set_http_middleware(
            ServiceBuilder::new()
                .layer(cors)
                .layer(jwt_auth)
                .layer(ProxyGetRequestLayer::new(
                    [("/health", "health"), ("/live", "live"), ("/ready", "ready")]
                        .into_iter()
                        .chain(well_known_routes),
                )?)
                .layer_fn(HttpTracingService::new),
        )
        .set_rpc_middleware(RpcServiceBuilder::new().layer_fn(RpcMetricsService::new))
        .build((config.server.address, config.server.port))
        .await?;
    let addr = server.local_addr()?;
    info!(%addr, "Started relay service");
    info!("Transaction signers: {}", signer_addresses.iter().join(", "));
    info!("Quote signer key: {}", quote_signer_addr);
    info!("Funder signer key: {}", funder_signer.address());

    // version and other information as a metric
    counter!(
        "relay.info",
        "version" => RELAY_LONG_VERSION,
        "orchestrator" => config.orchestrator.to_string(),
        "delegation_proxy" => config.delegation_proxy.to_string(),
        "simulator" => config.simulator.to_string(),
        "funder" => config.funder.to_string(),
        "fee_recipient" => config.fee_recipient.to_string()
    )
    .absolute(1);

    if let Some(account_rpc) = account_rpc {
        rpc.merge(account_rpc).expect("could not merge rpc modules");
    } else {
        warn!("No e-mail provider configured.");
    }

    Ok(RelayHandle {
        local_addr: addr,
        server: server.start(rpc),
        chains,
        storage,
        metrics,
        price_oracle,
    })
}

/// Fail-closed guard on the dev sponsorship escape hatch (`auth.dev_api_key`):
/// when the hatch is enabled, its subject MUST carry a dedicated, finite (> 0)
/// per-subject quota override, so the exposed static key is always tightly
/// capped — even though it runs against the shared prod relay (the dev app calls
/// the single hosted relay serving mainnet). Refusing to start otherwise
/// prevents an uncapped bypass token from going live. The key stays STATIC
/// (rotated via Infisical → `RELAY_DEV_API_KEY`), never a JWKS-signing secret;
/// see the JWKS-rotation incident.
fn assert_dev_hatch_safe(config: &RelayConfig) -> eyre::Result<()> {
    let Some(auth) = config.auth.as_ref() else { return Ok(()) };
    if auth.dev_api_key.is_none() {
        return Ok(());
    }

    let dev_subject = auth.dev_subject.clone().unwrap_or_else(|| "dev-local".to_string());
    check_dev_hatch_capped(&dev_subject, &config.sponsorship.quota_overrides)?;

    warn!(
        subject = %dev_subject,
        "auth.dev_api_key is ENABLED — dev sponsorship escape hatch active (capped by its \
         quota_override)."
    );
    Ok(())
}

/// Pure core of [`assert_dev_hatch_safe`]: the dev subject must have a finite,
/// non-zero per-subject quota override.
fn check_dev_hatch_capped(
    dev_subject: &str,
    quota_overrides: &std::collections::HashMap<String, u128>,
) -> eyre::Result<()> {
    match quota_overrides.get(dev_subject) {
        Some(&wei) if wei > 0 => Ok(()),
        _ => eyre::bail!(
            "FATAL: auth.dev_api_key is set but dev_subject '{dev_subject}' has no dedicated \
             quota_override (> 0 wei); refusing to start. Add \
             sponsorship.quota_overrides['{dev_subject}'] to tightly cap the exposed dev key."
        ),
    }
}

/// Registers each configured `/.well-known/` document as a bare RPC method and
/// returns the `(path, method)` pairs that let the GET proxy serve it, the same
/// way `/health` is served. The proxy answers with the method's result as the
/// body, typed `application/json`, which is what iOS and Android fetch.
fn register_well_known<C: Send + Sync + 'static>(
    rpc: &mut RpcModule<C>,
    config: &WellKnownConfig,
) -> eyre::Result<Vec<(&'static str, &'static str)>> {
    let mut routes = Vec::new();
    for (path, method, doc) in [
        (
            "/.well-known/apple-app-site-association",
            "wellKnown_appleAppSiteAssociation",
            &config.apple_app_site_association,
        ),
        ("/.well-known/assetlinks.json", "wellKnown_assetlinks", &config.assetlinks),
    ] {
        let Some(doc) = doc.clone() else { continue };
        rpc.register_method(method, move |_, _, _| Ok::<_, ErrorObjectOwned>(doc.clone()))?;
        routes.push((path, method));
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::{assert_dev_hatch_safe, check_dev_hatch_capped, register_well_known};
    use crate::config::{AuthConfig, RelayConfig, WellKnownConfig};
    use jsonrpsee::{
        RpcModule,
        server::{Server, middleware::http::ProxyGetRequestLayer},
    };
    use std::collections::HashMap;
    use tower::ServiceBuilder;

    #[tokio::test]
    async fn well_known_documents_are_served_on_get() {
        let config: WellKnownConfig = serde_yaml::from_str(
            "apple_app_site_association:\n  webcredentials:\n    apps: [\"TEAM.com.example.app\"]\n",
        )
        .unwrap();
        let mut rpc = RpcModule::new(());
        let routes = register_well_known(&mut rpc, &config).unwrap();
        let server = Server::builder()
            .set_http_middleware(
                ServiceBuilder::new().layer(ProxyGetRequestLayer::new(routes).unwrap()),
            )
            .build("127.0.0.1:0")
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.start(rpc);

        let res = reqwest::get(format!("http://{addr}/.well-known/apple-app-site-association"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let content_type = res.headers()[http::header::CONTENT_TYPE].to_str().unwrap().to_owned();
        assert!(content_type.starts_with("application/json"), "{content_type}");
        let body: serde_json::Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
        assert_eq!(Some(body), config.apple_app_site_association);

        // A document left out of the config gets no route.
        let res = reqwest::get(format!("http://{addr}/.well-known/assetlinks.json")).await.unwrap();
        assert_eq!(res.status(), 405);

        handle.stop().unwrap();
    }

    #[test]
    fn dev_hatch_ok_when_subject_has_positive_override() {
        // A dedicated cap makes the hatch legal — mainnet chains no longer
        // matter, since the dev app calls the shared prod relay.
        let overrides = HashMap::from([("dev-local".to_string(), 1_000_000_000_000_000u128)]);
        assert!(check_dev_hatch_capped("dev-local", &overrides).is_ok());
    }

    #[test]
    fn dev_api_key_without_dev_subject_quota_fails_closed() {
        // Missing override -> refuse.
        assert!(check_dev_hatch_capped("dev-local", &HashMap::new()).is_err());
        // Zero override is not a real cap -> refuse.
        let zero = HashMap::from([("dev-local".to_string(), 0u128)]);
        let err = check_dev_hatch_capped("dev-local", &zero).unwrap_err().to_string();
        assert!(err.contains("dev-local"));
        assert!(err.contains("quota_override"));
    }

    fn cfg_with_hatch(dev_subject: Option<&str>, override_wei: Option<u128>) -> RelayConfig {
        let mut config = RelayConfig {
            auth: Some(AuthConfig {
                jwks_url: "http://127.0.0.1:1/jwks".to_string(),
                dev_api_key: Some("dev-secret".to_string()),
                dev_subject: dev_subject.map(str::to_owned),
            }),
            ..Default::default()
        };
        if let Some(wei) = override_wei {
            let subject = dev_subject.unwrap_or("dev-local").to_string();
            config.sponsorship.quota_overrides.insert(subject, wei);
        }
        config
    }

    #[test]
    fn assert_dev_hatch_safe_end_to_end() {
        // Default subject ("dev-local") + a cap -> ok.
        assert!(assert_dev_hatch_safe(&cfg_with_hatch(None, Some(1_000_000_000_000_000))).is_ok());
        // Explicit subject + its cap -> ok.
        assert!(
            assert_dev_hatch_safe(&cfg_with_hatch(Some("dev-x"), Some(1_000_000_000_000_000)))
                .is_ok()
        );
        // Hatch on but no cap -> refuse.
        assert!(assert_dev_hatch_safe(&cfg_with_hatch(None, None)).is_err());
        // No auth block at all -> ok (hatch off).
        assert!(assert_dev_hatch_safe(&RelayConfig::default()).is_ok());
    }
}
