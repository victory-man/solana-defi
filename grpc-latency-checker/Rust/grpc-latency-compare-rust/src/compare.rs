use hyper_util::rt::TokioIo;
use crate::tempo::transaction_stream_client::TransactionStreamClient;
use crate::tempo::StartStreamV2;
use solana_sdk::pubkey;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use tokio::net::UnixStream;
use tonic::transport::Uri;
use tonic_health::pb::health_client::HealthClient;
use tower::service_fn;
use yellowstone_grpc_client::InterceptorXToken;
use yellowstone_grpc_proto::geyser::geyser_client::GeyserClient;
use {
    chrono::Utc,
    dotenv::dotenv,
    futures::{sink::SinkExt, stream::StreamExt},
    log::{error, info},
    maplit::hashmap,
    serde::Deserialize,
    solana_entry::entry::Entry,
    solana_sdk::signature::Signature,
    std::{
        collections::{HashMap, HashSet},
        env,
        str::FromStr,
        sync::Arc,
        time::Duration,
    },
    tokio::sync::{mpsc, oneshot},
    tonic::{metadata::MetadataValue, transport::Endpoint, Request},
    yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient},
    yellowstone_grpc_proto::prelude::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterBlocksMeta, SubscribeRequestFilterTransactions,
    },
};

mod tempo {
    tonic::include_proto!("tempo");
}
mod bloxroute {
    tonic::include_proto!("bloxroute");
}

#[derive(Debug, Deserialize, Clone)]
struct StreamConfig {
    uri: String,
    x_token: Option<String>,
    kind: Option<u8>,
}

#[derive(Debug, Clone)]
struct Args {
    yellowstone_stream_configs: Option<Vec<StreamConfig>>,
    shred_stream_configs: Option<Vec<StreamConfig>>,
    timeout_dur: u64,
}

impl Args {
    fn from_env() -> Self {
        dotenv().ok();
        env::set_var(
            env_logger::DEFAULT_FILTER_ENV,
            env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
        );
        env_logger::init();

        let yellowstone_stream_configs =
            parse_json_vec_env::<StreamConfig>("YELLOWSTONE_STREAM_CONFIGS");
        let shred_stream_configs = parse_json_vec_env::<StreamConfig>("SHRED_STREAM_CONFIGS");

        let timeout_dur = env::var("TIMEOUT_DUR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);

        Args {
            yellowstone_stream_configs: Some(yellowstone_stream_configs),
            shred_stream_configs: Some(shred_stream_configs),
            timeout_dur,
        }
    }
}

fn parse_json_vec_env<T: for<'de> Deserialize<'de>>(key: &str) -> Vec<T> {
    env::var(key)
        .ok()
        .and_then(|v| serde_json::from_str::<Vec<T>>(&v).ok())
        .unwrap_or_default()
}

#[derive(Eq, Hash, PartialEq, Default, Debug)]
struct Timing {
    sig: String,
    timestamp: u64,
    node: Arc<String>,
}
#[derive(Default, Debug)]
struct LatencyChecker {
    txns: HashMap<String, HashSet<Timing>>,
    blocks: HashMap<String, HashSet<Timing>>,
}
struct LatencyCheckerInput {
    signature: String,
    timestamp: u64,
    node: Arc<String>,
    m_type: u8,
}

#[derive(Default, Debug)]
struct LatencyReportLag {
    count: u64,
    time_taken: u64,
}
impl LatencyChecker {
    // m_type 0
    fn add_txn(&mut self, signature: String, timestamp: u64, node: Arc<String>) {
        let timing = Timing {
            sig: signature.clone(),
            timestamp,
            node,
        };
        if let Some(set) = self.txns.get_mut(&signature) {
            set.insert(timing);
        } else {
            let mut set = HashSet::new();
            set.insert(timing);
            self.txns.insert(signature, set);
        }
    }
    // m_type 1
    fn add_block(&mut self, block: String, timestamp: u64, node: Arc<String>) {
        let timing = Timing {
            sig: block.clone(),
            timestamp,
            node,
        };
        if let Some(set) = self.blocks.get_mut(&block) {
            set.insert(timing);
        } else {
            let mut set = HashSet::new();
            set.insert(timing);
            self.blocks.insert(block, set);
        }
    }

    async fn listen_messages(&mut self, mut m_rx: mpsc::Receiver<LatencyCheckerInput>) {
        while let Some(m) = m_rx.recv().await {
            if m.m_type == 0 {
                info!(
                    "received txn {} at {} from node {}",
                    m.signature, m.timestamp, m.node
                );
                self.add_txn(m.signature, m.timestamp, m.node);
            } else {
                self.add_block(m.signature, m.timestamp, m.node);
            }
        }
    }

    fn get_report(&self) {
        let mut txns_compare: HashMap<Arc<String>, LatencyReportLag> = HashMap::new(); // map of node vs (fastest,slowest) between others
        for v in self.txns.values() {
            let mut values: Vec<_> = v.into_iter().collect();
            values.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            let fastest = values.first();
            let slowest = values.last();

            if let Some(f) = fastest {
                let s_tmp = slowest.map(|s| s.timestamp).unwrap_or(0);

                if f.timestamp == s_tmp {
                    continue;
                }
                info!("Fastes: {}, slow: {}", f.timestamp, s_tmp);
                if let Some(c) = txns_compare.get_mut(&f.node) {
                    c.count += 1;
                    c.time_taken += s_tmp - f.timestamp;
                } else {
                    txns_compare.insert(
                        f.node.clone(),
                        LatencyReportLag {
                            count: 1,
                            time_taken: s_tmp - f.timestamp,
                        },
                    );
                }
            }
        }

        info!("Final results:");
        info!("----------  Transactions --------");
        for (k, v) in txns_compare {
            info!(
                "{:?}, count: {}, avg_gain: {}",
                k,
                v.count,
                v.time_taken / v.count
            );
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args = Args::from_env();
    info!("Args: {:?}", args);
    let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(args.timeout_dur));

    let mut latency_checker = LatencyChecker::default();

    let mut shutdown_sig = Vec::new();
    let (m_tx, m_rx) = mpsc::channel(100_000);

    match args.yellowstone_stream_configs {
        Some(yellowstone_stream_configs) => {
            for yellowstone_stream_config in yellowstone_stream_configs {
                let token = yellowstone_stream_config.x_token.clone();
                let (tx, rx) = oneshot::channel();
                shutdown_sig.push(tx);
                let m_tx = m_tx.clone();

                info!(
                    "starting yellowstone grpc stream{}",
                    yellowstone_stream_config.uri
                );
                let kind = yellowstone_stream_config.kind.unwrap_or(0);
                if kind == 2 {
                    tokio::spawn(async move {
                        bloxroute_grpc_message_handler(
                            rx,
                            yellowstone_stream_config.uri,
                            token,
                            m_tx,
                        )
                        .await;
                    });
                } else if kind == 1 {
                    tokio::spawn(async move {
                        temp_grpc_message_handler(rx, yellowstone_stream_config.uri, token, m_tx)
                            .await;
                    });
                } else {
                    tokio::spawn(async move {
                        grpc_message_handler(rx, yellowstone_stream_config.uri, token, m_tx).await;
                    });
                }
            }
        }
        None => {}
    }

    tokio::select! {
        _ = latency_checker.listen_messages(m_rx) => {}
        _ = timeout => {
            for sig in shutdown_sig {
                _ = sig.send(true);
            }
            latency_checker.get_report();
        }
    }
}

async fn bloxroute_grpc_message_handler(
    timeout: oneshot::Receiver<bool>,
    endpoint: String,
    token: Option<String>,
    m_tx: mpsc::Sender<LatencyCheckerInput>,
) {
    let mut client =
        bloxroute::tx_streamer_service_client::TxStreamerServiceClient::connect(endpoint.clone())
            .await
            .unwrap();
    let endpoint = Arc::new(endpoint);
    // Send request to start stream
    let auth_token = token.unwrap();
    let start_stream_request = bloxroute::StreamTransactionsRequest {
        accounts: vec!["pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".to_string()],
    };
    let mut start_stream_request = Request::new(start_stream_request);
    start_stream_request.metadata_mut().insert("authorization", auth_token.parse().unwrap());
    let mut stream = client
        .stream_transactions(start_stream_request)
        .await
        .unwrap();
    tokio::select! {
        _ = timeout => {
            println!("Timeout reached, ending stream...");
        }
        _ = async {
            while let Ok(Some(tx)) = stream.get_mut().message().await {
                let current_time_millis = Utc::now().timestamp_millis() as u64;
                // info!("received txn {} at {} from node {}", sig, current_time_millis, endpoint);
                let tx = bincode::deserialize::<VersionedTransaction>(tx.data.as_ref()).unwrap();
                let sig = tx.signatures.get(0).unwrap().to_string();
                _ = m_tx.send(LatencyCheckerInput{
                    signature: sig,
                    timestamp: current_time_millis,
                    node: endpoint.clone(),
                    m_type: 0,
                }).await;
            }
        } => {},
    }
}

async fn temp_grpc_message_handler(
    timeout: oneshot::Receiver<bool>,
    endpoint: String,
    token: Option<String>,
    m_tx: mpsc::Sender<LatencyCheckerInput>,
) {
    let mut client = TransactionStreamClient::connect(endpoint.clone())
        .await
        .unwrap();
    let endpoint = Arc::new(endpoint);
    // Send request to start stream
    let auth_token = token.unwrap();
    let pump = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
    let start_stream_request = StartStreamV2 {
        auth_token,
        static_account_filter: vec![pump.to_bytes().to_vec()],
    };
    let mut stream = client
        .open_transaction_stream_v2(start_stream_request)
        .await
        .unwrap();
    tokio::select! {
        _ = timeout => {
            println!("Timeout reached, ending stream...");
        }
        _ = async {
            while let Ok(Some(tx)) = stream.get_mut().message().await {
                let current_time_millis = Utc::now().timestamp_millis() as u64;
                // info!("received txn {} at {} from node {}", sig, current_time_millis, endpoint);
                // let sig = format!("{}_{}", tx.slot, tx.index);
                let tx = bincode::deserialize::<VersionedTransaction>(tx.payload.as_ref()).unwrap();
                let sig = tx.signatures.get(0).unwrap().to_string();
                _ = m_tx.send(LatencyCheckerInput{
                    signature: sig,
                    timestamp: current_time_millis,
                    node: endpoint.clone(),
                    m_type: 0,
                }).await;
            }
        } => {},
    }
}

async fn grpc_message_handler(
    timeout: oneshot::Receiver<bool>,
    endpoint: String,
    token: Option<String>,
    m_tx: mpsc::Sender<LatencyCheckerInput>,
) {
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.clone())
        .unwrap()
        .x_token(token.clone())
        .unwrap()
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .unwrap()
        .send_compressed(yellowstone_grpc_proto::tonic::codec::CompressionEncoding::Gzip)
        .accept_compressed(yellowstone_grpc_proto::tonic::codec::CompressionEncoding::Gzip);
    let channel = builder.endpoint.connect_with_connector(service_fn(|_: Uri| async {
        Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect("/var/run/sol.sock").await?))
    })).await.unwrap();

    let interceptor = InterceptorXToken {
        x_token: builder.x_token,
        x_request_snapshot: builder.x_request_snapshot,
    };

    let mut geyser = GeyserClient::with_interceptor(channel.clone(), interceptor.clone());
    if let Some(encoding) = builder.send_compressed {
        geyser = geyser.send_compressed(encoding);
    }
    if let Some(encoding) = builder.accept_compressed {
        geyser = geyser.accept_compressed(encoding);
    }
    if let Some(limit) = builder.max_decoding_message_size {
        geyser = geyser.max_decoding_message_size(limit);
    }
    if let Some(limit) = builder.max_encoding_message_size {
        geyser = geyser.max_encoding_message_size(limit);
    }

    let mut client = GeyserGrpcClient::new(
        HealthClient::with_interceptor(channel, interceptor),
        geyser,
    );

    let (mut subscribe_tx, mut stream) = client.subscribe().await.unwrap();
    let endpoint = Arc::new(endpoint);

    let commitment: CommitmentLevel = CommitmentLevel::default();
    let mut transactions = HashMap::new();
    transactions.insert(
        "program".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: vec!["pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".to_string()],
            account_required: vec!["pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".to_string()],
            ..SubscribeRequestFilterTransactions::default()
        },
    );
    subscribe_tx
        .send(SubscribeRequest {
            slots: HashMap::new(),
            accounts: HashMap::new(),
            transactions,
            transactions_status: hashmap! { "".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: Vec::new(),
                account_exclude: Vec::new(),
                account_required: Vec::new(),
            } },
            entry: HashMap::new(),
            blocks: HashMap::new(),
            blocks_meta: hashmap! { "".to_owned() => SubscribeRequestFilterBlocksMeta {} },
            commitment: Some(commitment as i32),
            accounts_data_slice: vec![],
            ping: None,
            from_slot: None,
        })
        .await
        .unwrap();
    tokio::select! {
        _ = timeout => {
            println!("Timeout reached, ending stream...");
        }
        _ = async {
            while let Some(message) = stream.next().await {
                match message {
                    Ok(msg) => match msg.update_oneof {
                        Some(UpdateOneof::TransactionStatus(tx)) => {
                            let sig = Signature::try_from(tx.signature.as_slice())
                                .expect("valid signature from transaction")
                                .to_string();
                            let current_time_millis = Utc::now().timestamp_millis() as u64;
                            // info!("received txn {} at {} from node {}", sig, current_time_millis, endpoint);
                            // let sig = format!("{}_{}", tx.slot, tx.index);
                            _ = m_tx.send(LatencyCheckerInput{
                                signature: sig,
                                timestamp: current_time_millis,
                                node: endpoint.clone(),
                                m_type: 0,
                            }).await;
                        }
                        // Some(UpdateOneof::BlockMeta(block)) => {
                        //     let current_time_millis = Utc::now().timestamp_millis() as u64;
                        //     info!("received block {} at {}", block.blockhash, current_time_millis);
                        //     _ = m_tx.send(LatencyCheckerInput {
                        //         signature: block.blockhash,
                        //         timestamp:current_time_millis,
                        //         node: endpoint.clone(),
                        //         m_type: 1,
                        //     }).await;
                        // }
                        _ => {}
                    },
                    Err(error) => {
                        error!("stream error: {error:?}");
                        break;
                    }
                }
            }
        } => {}
    }
}