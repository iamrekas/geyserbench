use std::{
    collections::HashMap,
    error::Error,
    sync::{Arc, Mutex},
};

use futures_util::{stream::StreamExt, sink::SinkExt};
use tokio::{sync::broadcast, task};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::{
    geyser::{
        subscribe_update::UpdateOneof, SubscribeRequest, SubscribeRequestPing,
        SubscribeRequestFilterAccounts,
    },
    prelude::SubscribeRequestFilterTransactions,
    tonic::transport::ClientTlsConfig,
};

use crate::{
    config::{Config, Endpoint},
    utils::{Comparator, TransactionData, get_current_timestamp, open_log_file, write_log_entry},
};

use super::GeyserProvider;

pub struct YellowstoneAccountsProvider;

// AIDEV-NOTE: Shared structure for cross-endpoint account tracking
lazy_static::lazy_static! {
    static ref GLOBAL_ACCOUNT_TRACKER: Arc<Mutex<HashMap<String, StreamLatencyData>>> = Arc::new(Mutex::new(HashMap::new()));
}

impl GeyserProvider for YellowstoneAccountsProvider {
    fn process(
        &self,
        endpoint: Endpoint,
        config: Config,
        shutdown_tx: broadcast::Sender<()>,
        shutdown_rx: broadcast::Receiver<()>,
        start_time: f64,
        comparator: Arc<Mutex<Comparator>>,
    ) -> task::JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> {
        task::spawn(async move {
            process_yellowstone_accounts_endpoint(
                endpoint,
                config,
                shutdown_tx,
                shutdown_rx,
                start_time,
                comparator,
            )
                .await
        })
    }
}

// AIDEV-NOTE: Track latency differences between account and transaction streams
#[derive(Debug, Clone)]
struct StreamLatencyData {
    signature: String,
    account_timestamp: Option<f64>,
    transaction_timestamp: Option<f64>,
    account_endpoint: Option<String>,  // Track which endpoint saw account first
    transaction_endpoint: Option<String>, // Track which endpoint saw transaction first
}

async fn process_yellowstone_accounts_endpoint(
    endpoint: Endpoint,
    config: Config,
    shutdown_tx: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
    start_time: f64,
    comparator: Arc<Mutex<Comparator>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut transaction_count = 0;
    let mut account_update_count = 0;
    
    // Track latencies for both streams
    let mut stream_latencies: HashMap<String, StreamLatencyData> = HashMap::new();
    
    let mut log_file = open_log_file(&format!("{}_dual_stream", endpoint.name))?;

    log::info!(
        "[{}] Connecting to endpoint for dual stream tracking: {}",
        endpoint.name,
        endpoint.url
    );

    let mut client = GeyserGrpcClient::build_from_shared(endpoint.url)?
        .x_token(Some(endpoint.x_token))?
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        .connect()
        .await?;

    log::info!("[{}] Connected successfully", endpoint.name);

    let (mut subscribe_tx, mut stream) = client.subscribe().await?;
    let commitment: yellowstone_grpc_proto::geyser::CommitmentLevel = config.commitment.into();
    
    log::info!(
        "[{}] Subscribing to account {} with commitment {:?}",
        endpoint.name,
        config.account,
        commitment
    );

    // Subscribe to both transactions and accounts for the same account
    let mut transactions = HashMap::new();
    transactions.insert(
        "account".to_string(),
        SubscribeRequestFilterTransactions {
            account_include: vec![config.account.clone()],
            account_exclude: vec![],
            account_required: vec![],
            ..Default::default()
        },
    );

    let mut accounts = HashMap::new();
    // Subscribe to the specific account - try without txn_signature filter first
    accounts.insert(
        "account".to_string(),
        SubscribeRequestFilterAccounts {
            // account: vec![config.account.clone()],
            account:vec![],
            owner: vec![],
            filters: vec![],
            nonempty_txn_signature: None, // Try without filter first
        },
    );

    let subscribe_request = SubscribeRequest {
        slots: HashMap::default(),
        accounts,
        transactions,
        transactions_status: HashMap::default(),
        entry: HashMap::default(),
        blocks: HashMap::default(),
        blocks_meta: HashMap::default(),
        commitment: Some(commitment as i32),
        accounts_data_slice: Vec::default(),
        ping: None,
        from_slot: None,
    };
    
    log::debug!("[{}] Sending subscribe request with {} account filters and {} transaction filters", 
        endpoint.name, 
        subscribe_request.accounts.len(),
        subscribe_request.transactions.len()
    );
    
    subscribe_tx.send(subscribe_request).await?;

    'ploop: loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                log::info!("[{}] Received stop signal...", endpoint.name);
                break;
            }

            message = stream.next() => {
                match message {
                    Some(Ok(msg)) => {
                        match msg.update_oneof {
                            Some(UpdateOneof::Transaction(tx_msg)) => {
                                if let Some(tx) = tx_msg.transaction {
                                    let accounts = tx.transaction.clone().unwrap().message.unwrap().account_keys
                                        .iter()
                                        .map(|key| bs58::encode(key).into_string())
                                        .collect::<Vec<String>>();

                                    if accounts.contains(&config.account) {
                                        let timestamp = get_current_timestamp();
                                        let signature = bs58::encode(&tx.transaction.unwrap().signatures[0]).into_string();

                                        // Track transaction stream timestamp locally
                                        let entry = stream_latencies.entry(signature.clone()).or_insert(StreamLatencyData {
                                            signature: signature.clone(),
                                            account_timestamp: None,
                                            transaction_timestamp: None,
                                            account_endpoint: None,
                                            transaction_endpoint: None,
                                        });
                                        entry.transaction_timestamp = Some(timestamp);
                                        if entry.transaction_endpoint.is_none() {
                                            entry.transaction_endpoint = Some(endpoint.name.clone());
                                        }
                                        
                                        // Also track globally for cross-endpoint comparison
                                        {
                                            let mut global_tracker = GLOBAL_ACCOUNT_TRACKER.lock().unwrap();
                                            let global_entry = global_tracker.entry(signature.clone()).or_insert(StreamLatencyData {
                                                signature: signature.clone(),
                                                account_timestamp: None,
                                                transaction_timestamp: None,
                                                account_endpoint: None,
                                                transaction_endpoint: None,
                                            });
                                            if global_entry.transaction_timestamp.is_none() || timestamp < global_entry.transaction_timestamp.unwrap() {
                                                global_entry.transaction_timestamp = Some(timestamp);
                                                global_entry.transaction_endpoint = Some(endpoint.name.clone());
                                            }
                                        }

                                        // Log transaction received
                                        write_log_entry(&mut log_file, timestamp, &format!("{}_TX", endpoint.name), &signature)?;

                                        // Check if we have both streams for this signature
                                        if let Some(account_ts) = entry.account_timestamp {
                                            let diff = timestamp - account_ts;
                                            log::info!(
                                                "[{}] Dual stream - TX: {:.3}, Acct: {:.3}, Diff: {:.3}ms - {}",
                                                endpoint.name,
                                                timestamp,
                                                account_ts,
                                                diff * 1000.0,
                                                signature
                                            );
                                        }

                                        let mut comp = comparator.lock().unwrap();
                                        comp.add(
                                            endpoint.name.clone(),
                                            TransactionData {
                                                timestamp,
                                                signature: signature.clone(),
                                                start_time,
                                            },
                                        );

                                        if comp.get_valid_count() == config.transactions as usize {
                                            log::info!("Endpoint {} shutting down after {} transactions seen",
                                                endpoint.name, transaction_count);
                                            
                                            // Print final statistics
                                            print_stream_statistics(&stream_latencies, &endpoint.name);
                                            
                                            shutdown_tx.send(()).unwrap();
                                            break 'ploop;
                                        }

                                        transaction_count += 1;
                                    }
                                }
                            },
                            Some(UpdateOneof::Account(account_msg)) => {
                                // AIDEV-NOTE: Process ALL account updates that have txn_signature
                                if let Some(account_info) = account_msg.account {
                                    let account_key = bs58::encode(&account_info.pubkey).into_string();
                                    account_update_count += 1;
                                    
                                    // Check if account update has txn_signature
                                    if let Some(txn_sig_bytes) = account_info.txn_signature {
                                        let timestamp = get_current_timestamp();
                                        let signature = bs58::encode(&txn_sig_bytes).into_string();
                                        
                                        // Only log first few to avoid spam
                                        if account_update_count <= 10 {
                                            log::info!(
                                                "[{}] Account update #{} for {} with sig {} at {:.3}",
                                                endpoint.name,
                                                account_update_count,
                                                &account_key[0..8], // First 8 chars of account
                                                &signature[0..8], // First 8 chars of signature
                                                timestamp
                                            );
                                        }
                                        
                                        // Track account stream timestamp locally
                                        let entry = stream_latencies.entry(signature.clone()).or_insert(StreamLatencyData {
                                            signature: signature.clone(),
                                            account_timestamp: None,
                                            transaction_timestamp: None,
                                            account_endpoint: None,
                                            transaction_endpoint: None,
                                        });
                                        entry.account_timestamp = Some(timestamp);
                                        if entry.account_endpoint.is_none() {
                                            entry.account_endpoint = Some(endpoint.name.clone());
                                        }
                                        
                                        // Also track globally for cross-endpoint comparison
                                        {
                                            let mut global_tracker = GLOBAL_ACCOUNT_TRACKER.lock().unwrap();
                                            let global_entry = global_tracker.entry(signature.clone()).or_insert(StreamLatencyData {
                                                signature: signature.clone(),
                                                account_timestamp: None,
                                                transaction_timestamp: None,
                                                account_endpoint: None,
                                                transaction_endpoint: None,
                                            });
                                            if global_entry.account_timestamp.is_none() || timestamp < global_entry.account_timestamp.unwrap() {
                                                global_entry.account_timestamp = Some(timestamp);
                                                global_entry.account_endpoint = Some(endpoint.name.clone());
                                            }
                                        }
                                        
                                        // Log account update received
                                        write_log_entry(&mut log_file, timestamp, &format!("{}_ACCT", endpoint.name), &signature)?;
                                        
                                        // Check if we have both streams for this signature
                                        if let Some(tx_ts) = entry.transaction_timestamp {
                                            let diff = tx_ts - timestamp;
                                            log::info!(
                                                "[{}] Dual stream matched! Acct: {:.3}, TX: {:.3}, TX was {:.3}ms {} - sig: {}",
                                                endpoint.name,
                                                timestamp,
                                                tx_ts,
                                                diff.abs() * 1000.0,
                                                if diff > 0.0 { "later" } else { "earlier" },
                                                &signature[0..8]
                                            );
                                        }
                                    }
                                }
                            },
                            Some(UpdateOneof::Ping(_)) => {
                                subscribe_tx
                                    .send(SubscribeRequest {
                                        ping: Some(SubscribeRequestPing { id: 1 }),
                                        ..Default::default()
                                    })
                                    .await?;
                            },
                            Some(other) => {
                                let update_type = match other {
                                    UpdateOneof::Slot(_) => "Slot",
                                    UpdateOneof::TransactionStatus(_) => "TransactionStatus",
                                    UpdateOneof::Block(_) => "Block",
                                    UpdateOneof::BlockMeta(_) => "BlockMeta",
                                    UpdateOneof::Entry(_) => "Entry",
                                    _ => "Unknown",
                                };
                                log::debug!("[{}] Received other update type: {}", endpoint.name, update_type);
                            },
                            None => {
                                log::trace!("[{}] Received empty update", endpoint.name);
                            }
                        }
                    },
                    Some(Err(e)) => {
                        log::error!("[{}] Error receiving message: {:?}", endpoint.name, e);
                        break;
                    },
                    None => {
                        log::info!("[{}] Stream closed", endpoint.name);
                        break;
                    }
                }
            }
        }
    }

    log::info!(
        "[{}] Stream closed. Total transactions: {}, Account updates: {}",
        endpoint.name, transaction_count, account_update_count
    );
    
    // Print global statistics when last endpoint shuts down
    static SHUTDOWN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if SHUTDOWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
        print_global_statistics();
    }
    
    Ok(())
}

fn print_global_statistics() {
    let global_tracker = GLOBAL_ACCOUNT_TRACKER.lock().unwrap();
    
    log::info!("=== GLOBAL CROSS-ENDPOINT STATISTICS ===");
    log::info!("Total unique signatures tracked: {}", global_tracker.len());
    
    let mut account_endpoint_wins: HashMap<String, usize> = HashMap::new();
    let mut tx_endpoint_wins: HashMap<String, usize> = HashMap::new();
    let mut both_received = 0;
    let mut account_faster = 0;
    let mut tx_faster = 0;
    let mut timing_diffs = Vec::new();
    
    for (_, data) in global_tracker.iter() {
        // Count account endpoint wins
        if let Some(endpoint) = &data.account_endpoint {
            *account_endpoint_wins.entry(endpoint.clone()).or_insert(0) += 1;
        }
        
        // Count transaction endpoint wins
        if let Some(endpoint) = &data.transaction_endpoint {
            *tx_endpoint_wins.entry(endpoint.clone()).or_insert(0) += 1;
        }
        
        // Calculate stream timing differences
        if let (Some(acct_ts), Some(tx_ts)) = (data.account_timestamp, data.transaction_timestamp) {
            both_received += 1;
            let diff = tx_ts - acct_ts;
            timing_diffs.push(diff * 1000.0); // Convert to ms
            
            if acct_ts < tx_ts {
                account_faster += 1;
            } else {
                tx_faster += 1;
            }
        }
    }
    
    // Report endpoint wins
    log::info!("\n--- Account Stream First by Endpoint ---");
    for (endpoint, count) in account_endpoint_wins.iter() {
        let percentage = (*count as f64 / global_tracker.len() as f64) * 100.0;
        log::info!("{}: {} wins ({:.1}%)", endpoint, count, percentage);
    }
    
    log::info!("\n--- Transaction Stream First by Endpoint ---");
    for (endpoint, count) in tx_endpoint_wins.iter() {
        let percentage = (*count as f64 / global_tracker.len() as f64) * 100.0;
        log::info!("{}: {} wins ({:.1}%)", endpoint, count, percentage);
    }
    
    // Stream type comparison
    if both_received > 0 {
        timing_diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg_diff = timing_diffs.iter().sum::<f64>() / timing_diffs.len() as f64;
        let median_diff = timing_diffs[timing_diffs.len() / 2];
        
        log::info!("\n--- Account vs Transaction Stream Timing ---");
        log::info!("Signatures with both streams: {}", both_received);
        log::info!("Account stream faster: {} ({:.1}%)", 
            account_faster, 
            account_faster as f64 / both_received as f64 * 100.0
        );
        log::info!("Transaction stream faster: {} ({:.1}%)", 
            tx_faster,
            tx_faster as f64 / both_received as f64 * 100.0
        );
        log::info!("Average timing difference: {:.2}ms (positive = TX later)", avg_diff);
        log::info!("Median timing difference: {:.2}ms", median_diff);
        if !timing_diffs.is_empty() {
            log::info!("Min difference: {:.2}ms", timing_diffs[0]);
            log::info!("Max difference: {:.2}ms", timing_diffs[timing_diffs.len() - 1]);
        }
    }
}

fn print_stream_statistics(latencies: &HashMap<String, StreamLatencyData>, endpoint_name: &str) {
    let mut account_first_count = 0;
    let mut tx_first_count = 0;
    let mut both_received = 0;
    let mut total_diff = 0.0;
    let mut diffs = Vec::new();
    
    for (_, data) in latencies.iter() {
        if let (Some(acct_ts), Some(tx_ts)) = (data.account_timestamp, data.transaction_timestamp) {
            both_received += 1;
            let diff = (tx_ts - acct_ts).abs();
            total_diff += diff;
            diffs.push(diff * 1000.0); // Convert to milliseconds
            
            if acct_ts < tx_ts {
                account_first_count += 1;
            } else {
                tx_first_count += 1;
            }
        }
    }
    
    if both_received > 0 {
        diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg_diff = total_diff / both_received as f64 * 1000.0;
        let median = if diffs.len() > 0 {
            diffs[diffs.len() / 2]
        } else {
            0.0
        };
        
        log::info!("=== Stream Latency Statistics for {} ===", endpoint_name);
        log::info!("Total signatures tracked: {}", latencies.len());
        log::info!("Both streams received: {}", both_received);
        log::info!("Account stream first: {} ({:.1}%)", 
            account_first_count, 
            account_first_count as f64 / both_received as f64 * 100.0
        );
        log::info!("Transaction stream first: {} ({:.1}%)", 
            tx_first_count,
            tx_first_count as f64 / both_received as f64 * 100.0
        );
        log::info!("Average latency difference: {:.2}ms", avg_diff);
        log::info!("Median latency difference: {:.2}ms", median);
        if diffs.len() > 0 {
            log::info!("Min latency difference: {:.2}ms", diffs[0]);
            log::info!("Max latency difference: {:.2}ms", diffs[diffs.len() - 1]);
        }
    }
}