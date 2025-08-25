use std::{ error::Error, sync::{ Arc, Mutex }, io::Write };
use futures_util::StreamExt;
use tokio::{ sync::broadcast, task };

use crate::{
    config::{ Config, Endpoint },
    utils::{ Comparator, TransactionData, get_current_timestamp, open_log_file, write_log_entry },
};

use super::GeyserProvider;

pub mod shredstream {
    #![allow(clippy::clone_on_ref_ptr)]
    #![allow(clippy::missing_const_for_fn)]

    include!(concat!(env!("OUT_DIR"), "/shredstream.rs"));

    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("proto_descriptors");
}

use shredstream::{
    shredstream_proxy_client::ShredstreamProxyClient,
    SubscribeEntriesRequest,
    Entry,
};

pub struct ShredstreamProxyProvider;

impl GeyserProvider for ShredstreamProxyProvider {
    fn process(
        &self,
        endpoint: Endpoint,
        config: Config,
        shutdown_tx: broadcast::Sender<()>,
        shutdown_rx: broadcast::Receiver<()>,
        start_time: f64,
        comparator: Arc<Mutex<Comparator>>
    ) -> task::JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> {
        task::spawn(async move {
            process_shreds_endpoint(
                endpoint,
                config,
                shutdown_tx,
                shutdown_rx,
                start_time,
                comparator
            ).await
        })
    }
}

async fn process_shreds_endpoint(
    endpoint: Endpoint,
    config: Config,
    shutdown_tx: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
    start_time: f64,
    comparator: Arc<Mutex<Comparator>>
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut transaction_count = 0;
    let mut log_file = open_log_file(&endpoint.name)?;

    log::info!("[{}] Connecting to endpoint: {}", endpoint.name, endpoint.url);

    let mut client = ShredstreamProxyClient::connect(endpoint.url.clone()).await?;
    log::info!("[{}] Connected successfully", endpoint.name);

    // AIDEV-NOTE: SubscribeEntries doesn't require filters like SubscribeTransactions
    let request = SubscribeEntriesRequest {};
    
    let mut stream = client.subscribe_entries(request).await?.into_inner();

    'ploop: loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                log::info!("[{}] Received stop signal...", endpoint.name);
                break;
            }

            message = stream.next() => {
                if let Some(Ok(entry)) = message {
                    // Process Entry message
                    process_entry(
                        entry,
                        &endpoint,
                        &config,
                        &mut log_file,
                        &mut transaction_count,
                        start_time,
                        &comparator,
                        &shutdown_tx
                    ).await?;
                    
                    let comp = comparator.lock().unwrap();
                    if comp.get_valid_count() == config.transactions as usize {
                        log::info!("Endpoint {} shutting down after {} transactions seen and {} by all workers",
                            endpoint.name, transaction_count, config.transactions);
                        shutdown_tx.send(()).unwrap();
                        break 'ploop;
                    }
                } else {
                    log::warn!("[{}] Stream ended or error occurred", endpoint.name);
                    break;
                }
            }
        }
    }

    log::info!("[{}] Stream closed", endpoint.name);
    Ok(())
}

async fn process_entry(
    entry: Entry,
    endpoint: &Endpoint,
    config: &Config,
    log_file: &mut impl Write,
    transaction_count: &mut usize,
    start_time: f64,
    comparator: &Arc<Mutex<Comparator>>,
    _shutdown_tx: &broadcast::Sender<()>
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // AIDEV-NOTE: Entry contains serialized Vec<Entry> - need to deserialize
    use solana_entry::entry::Entry as SolanaEntry;
    
    let slot = entry.slot;
    let entries_bytes = entry.entries;
    
    // Deserialize the entries
    if let Ok(entries) = bincode::deserialize::<Vec<SolanaEntry>>(&entries_bytes) {
        for solana_entry in entries {
            // Process transactions in each entry
            for tx in solana_entry.transactions {
                // Get all account keys from the transaction
                let accounts: Vec<String> = match &tx.message {
                    solana_sdk::message::VersionedMessage::Legacy(msg) => {
                        msg.account_keys.iter().map(|key| key.to_string()).collect()
                    },
                    solana_sdk::message::VersionedMessage::V0(msg) => {
                        msg.account_keys.iter().map(|key| key.to_string()).collect()
                    }
                };
                
                if accounts.contains(&config.account) {
                    let timestamp = get_current_timestamp();
                    let signature = tx.signatures[0].to_string();
                    
                    write_log_entry(log_file, timestamp, &endpoint.name, &signature)?;
                    
                    let mut comp = comparator.lock().unwrap();
                    comp.add(
                        endpoint.name.clone(),
                        TransactionData {
                            timestamp,
                            signature: signature.clone(),
                            start_time,
                        },
                    );
                    
                    log::info!("[{:.3}] [{}] Slot: {} Signature: {}", 
                        timestamp, endpoint.name, slot, signature);
                    *transaction_count += 1;
                }
            }
        }
    } else {
        log::debug!("[{}] Failed to deserialize entries for slot {}", endpoint.name, slot);
    }
    
    Ok(())
}