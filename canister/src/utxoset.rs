use crate::address_utxoset::AddressUtxoSet;
use crate::{
    runtime::performance_counter,
    state::UtxoSet,
    types::{OutPoint, Slicing, Storable},
};
use bitcoin::{Address, Script, Transaction, TxOut, Txid};
use std::str::FromStr;

type Height = u32;

lazy_static::lazy_static! {
    static ref DUPLICATE_TX_IDS: [Vec<u8>; 2] = [
        Txid::from_str("d5d27987d2a3dfc724e359870c6644b40e497bdc0589a033220fe15429d88599").unwrap().to_vec(),
        Txid::from_str("e3bf3d07d4b0375638d5f1db5255fe07ba2c4cb067cd81b84ee974b6585fb468").unwrap().to_vec()
    ];
}

// The threshold at which time slicing kicks in.
// At the time of this writing it is equivalent to 80% of the maximum instructions limit.
const MAX_INSTRUCTIONS_THRESHOLD: u64 = 4_000_000_000;

/// Returns the `UtxoSet` of a given bitcoin address.
pub fn get_utxos<'a>(utxo_set: &'a UtxoSet, address: &'a str) -> AddressUtxoSet<'a> {
    AddressUtxoSet::new(address.to_string(), utxo_set)
}

/// Ingests a transaction into the given UTXO set at the given height.
///
/// NOTE: This method does a form of time-slicing to stay within the instruction limit, and
/// multiple calls may be required for the transaction to be ingested.
///
/// Returns a `Slicing` struct with a tuple containing (# inputs removed, # outputs inserted).
pub fn ingest_tx_with_slicing(
    utxo_set: &mut UtxoSet,
    tx: &Transaction,
    height: Height,
    start_input_idx: usize,
    start_output_idx: usize,
) -> Slicing<(usize, usize)> {
    if let Slicing::Paused(input_idx) = remove_inputs(utxo_set, tx, start_input_idx) {
        return Slicing::Paused((input_idx, 0));
    }

    if let Slicing::Paused(output_idx) = insert_outputs(utxo_set, tx, height, start_output_idx) {
        return Slicing::Paused((tx.input.len(), output_idx));
    }

    Slicing::Done
}

// Iterates over transaction inputs, starting from `start_idx`, and removes them from the UTXO set.
fn remove_inputs(utxo_set: &mut UtxoSet, tx: &Transaction, start_idx: usize) -> Slicing<usize> {
    if tx.is_coin_base() {
        return Slicing::Done;
    }

    for (input_idx, input) in tx.input.iter().enumerate().skip(start_idx) {
        if performance_counter() >= MAX_INSTRUCTIONS_THRESHOLD {
            return Slicing::Paused(input_idx);
        }

        // Remove the input from the UTXOs. The input *must* exist in the UTXO set.
        match utxo_set.utxos.remove(&(&input.previous_output).into()) {
            Some((txout, height)) => {
                if let Some(address) = Address::from_script(
                    &Script::from(txout.script_pubkey),
                    utxo_set.network.into(),
                ) {
                    let address = address.to_string();
                    let found = utxo_set
                        .address_to_outpoints
                        .remove(&(address, height, (&input.previous_output).into()).to_bytes());

                    assert!(
                        found.is_some(),
                        "Outpoint {:?} not found in the index.",
                        input.previous_output
                    );
                }
            }
            None => {
                panic!("Outpoint {:?} not found.", input.previous_output);
            }
        }
    }

    Slicing::Done
}

// Iterates over transaction outputs, starting from `start_idx`, and inserts them into the UTXO set.
fn insert_outputs(
    utxo_set: &mut UtxoSet,
    tx: &Transaction,
    height: Height,
    start_idx: usize,
) -> Slicing<usize> {
    for (vout, output) in tx.output.iter().enumerate().skip(start_idx) {
        if performance_counter() >= MAX_INSTRUCTIONS_THRESHOLD {
            return Slicing::Paused(vout);
        }

        if !(output.script_pubkey.is_provably_unspendable()) {
            insert_utxo(
                utxo_set,
                OutPoint::new(tx.txid().to_vec(), vout as u32),
                output.clone(),
                height,
            );
        }
    }

    Slicing::Done
}

// Inserts a UTXO at a given height into the given UTXO set.
// A UTXO is represented by the the tuple: (outpoint, output)
pub(crate) fn insert_utxo(
    utxo_set: &mut UtxoSet,
    outpoint: OutPoint,
    output: TxOut,
    height: Height,
) {
    // Insert the outpoint.
    if let Some(address) = Address::from_script(&output.script_pubkey, utxo_set.network.into()) {
        let address_str = address.to_string();

        // Due to a bug in the bitcoin crate, it is possible in some extremely rare cases
        // that `Address:from_script` succeeds even if the address is invalid.
        //
        // To get around this bug, we convert the address to a string, and verify that this
        // string is a valid address.
        //
        // See https://github.com/rust-bitcoin/rust-bitcoin/issues/995 for more information.
        if Address::from_str(&address_str).is_ok() {
            // Add the address to the index if we can parse it.
            utxo_set
                .address_to_outpoints
                .insert((address_str, height, outpoint.clone()).to_bytes(), vec![])
                .expect("insertion must succeed");
        }
    }

    let outpoint_already_exists = utxo_set
        .utxos
        .insert(outpoint.clone(), ((&output).into(), height));

    // Verify that we aren't overwriting a previously seen outpoint.
    // NOTE: There was a bug where there were duplicate transactions. These transactions
    // we overwrite.
    //
    // See: https://en.bitcoin.it/wiki/BIP_0030
    //      https://bitcoinexplorer.org/tx/d5d27987d2a3dfc724e359870c6644b40e497bdc0589a033220fe15429d88599
    //      https://bitcoinexplorer.org/tx/e3bf3d07d4b0375638d5f1db5255fe07ba2c4cb067cd81b84ee974b6585fb468
    if outpoint_already_exists && !DUPLICATE_TX_IDS.contains(&outpoint.txid.to_vec()) {
        panic!(
            "Cannot insert outpoint {:?} because it was already inserted. Block height: {}",
            outpoint, height
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_utils::random_p2pkh_address;
    use crate::types::Network;
    use bitcoin::blockdata::{opcodes::all::OP_RETURN, script::Builder};
    use bitcoin::{Network as BitcoinNetwork, OutPoint as BitcoinOutPoint, TxOut};
    use ic_btc_test_utils::TransactionBuilder;
    use ic_btc_types::Address as AddressStr;
    use std::collections::BTreeSet;

    // A succinct wrapper around `ingest_tx_with_slicing` for tests that don't need slicing.
    fn ingest_tx(utxo_set: &mut UtxoSet, tx: &Transaction, height: Height) {
        assert_eq!(
            ingest_tx_with_slicing(utxo_set, tx, height, 0, 0),
            Slicing::Done
        );
    }

    #[test]
    fn coinbase_tx_mainnet() {
        coinbase_test(Network::Mainnet);
    }

    #[test]
    fn coinbase_tx_testnet() {
        coinbase_test(Network::Testnet);
    }

    #[test]
    fn coinbase_tx_regtest() {
        coinbase_test(Network::Regtest);
    }

    fn coinbase_test(network: Network) {
        let address = random_p2pkh_address(network);

        let coinbase_tx = TransactionBuilder::coinbase()
            .with_output(&address, 1000)
            .build();

        let mut utxo = UtxoSet::new(network);
        ingest_tx(&mut utxo, &coinbase_tx, 0);

        assert_eq!(utxo.utxos.len(), 1);
        assert_eq!(
            get_utxos(&utxo, &address.to_string()).into_vec(None),
            vec![ic_btc_types::Utxo {
                outpoint: ic_btc_types::OutPoint {
                    txid: coinbase_tx.txid().to_vec(),
                    vout: 0,
                },
                value: 1000,
                height: 0,
            }]
        );
    }

    #[test]
    fn tx_without_outputs_leaves_utxo_set_unchanged() {
        for network in [Network::Mainnet, Network::Regtest, Network::Testnet].iter() {
            let mut utxo = UtxoSet::new(*network);

            // no output coinbase
            let mut coinbase_empty_tx = TransactionBuilder::coinbase().build();
            coinbase_empty_tx.output.clear();
            ingest_tx(&mut utxo, &coinbase_empty_tx, 0);

            assert!(utxo.utxos.is_empty());
            assert!(utxo.address_to_outpoints.is_empty());
        }
    }

    #[test]
    fn filter_provably_unspendable_utxos() {
        for network in [Network::Mainnet, Network::Regtest, Network::Testnet].iter() {
            let mut utxo = UtxoSet::new(*network);

            // op return coinbase
            let coinbase_op_return_tx = Transaction {
                output: vec![TxOut {
                    value: 50_0000_0000,
                    script_pubkey: Builder::new().push_opcode(OP_RETURN).into_script(),
                }],
                input: vec![],
                version: 1,
                lock_time: 0,
            };
            ingest_tx(&mut utxo, &coinbase_op_return_tx, 0);

            assert!(utxo.utxos.is_empty());
            assert!(utxo.address_to_outpoints.is_empty());
        }
    }

    #[test]
    fn spending_mainnet() {
        spending(Network::Mainnet);
    }

    #[test]
    fn spending_testnet() {
        spending(Network::Testnet);
    }

    #[test]
    fn spending_regtest() {
        spending(Network::Regtest);
    }

    fn spending(network: Network) {
        let address_1 = random_p2pkh_address(network);
        let address_2 = random_p2pkh_address(network);

        let mut utxo = UtxoSet::new(network);

        let coinbase_tx = TransactionBuilder::coinbase()
            .with_output(&address_1, 1000)
            .build();
        ingest_tx(&mut utxo, &coinbase_tx, 0);

        let expected = vec![ic_btc_types::Utxo {
            outpoint: ic_btc_types::OutPoint {
                txid: coinbase_tx.txid().to_vec(),
                vout: 0,
            },
            value: 1000,
            height: 0,
        }];

        assert_eq!(
            get_utxos(&utxo, &address_1.to_string()).into_vec(None),
            expected
        );
        assert_eq!(
            utxo.address_to_outpoints
                .iter()
                .map(|(k, _)| <(String, Height, OutPoint)>::from_bytes(k))
                .collect::<BTreeSet<_>>(),
            maplit::btreeset! {
                (address_1.to_string(), 0, OutPoint::new(coinbase_tx.txid().to_vec(), 0))
            }
        );

        // Spend the output to address 2.
        let tx = TransactionBuilder::new()
            .with_input(BitcoinOutPoint::new(coinbase_tx.txid(), 0))
            .with_output(&address_2, 1000)
            .build();
        ingest_tx(&mut utxo, &tx, 1);

        assert_eq!(
            get_utxos(&utxo, &address_1.to_string()).into_vec(None),
            vec![]
        );
        assert_eq!(
            get_utxos(&utxo, &address_2.to_string()).into_vec(None),
            vec![ic_btc_types::Utxo {
                outpoint: ic_btc_types::OutPoint {
                    txid: tx.txid().to_vec(),
                    vout: 0
                },
                value: 1000,
                height: 1
            }]
        );
        assert_eq!(
            utxo.address_to_outpoints
                .iter()
                .map(|(k, _)| <(String, Height, OutPoint)>::from_bytes(k))
                .collect::<BTreeSet<_>>(),
            maplit::btreeset! {
                (address_2.to_string(), 1, OutPoint::new(tx.txid().to_vec(), 0))
            }
        );
    }

    #[test]
    fn utxos_are_sorted_by_height() {
        let address = random_p2pkh_address(Network::Testnet).to_string();

        let mut utxo = UtxoSet::new(Network::Testnet);

        // Insert some entries into the map with different heights in some random order.
        for height in [17u32, 0, 31, 4, 2].iter() {
            utxo.address_to_outpoints
                .insert(
                    (address.clone(), *height, OutPoint::new(vec![0; 32], 0)).to_bytes(),
                    vec![],
                )
                .unwrap();
        }

        // Verify that the entries returned are sorted in descending height.
        assert_eq!(
            utxo.address_to_outpoints
                .range(address.to_bytes(), None)
                .map(|(k, _)| {
                    let (_, height, _) = <(AddressStr, Height, OutPoint)>::from_bytes(k);
                    height
                })
                .collect::<Vec<_>>(),
            vec![31, 17, 4, 2, 0]
        );
    }

    #[test]
    #[should_panic]
    fn inserting_same_outpoint_panics() {
        let network = Network::Testnet;
        let mut utxo_set = UtxoSet::new(network);
        let address = random_p2pkh_address(network);

        let tx_out_1 = TransactionBuilder::coinbase()
            .with_output(&address, 1000)
            .build()
            .output[0]
            .clone();

        let tx_out_2 = TransactionBuilder::coinbase()
            .with_output(&address, 2000)
            .build()
            .output[0]
            .clone();

        let outpoint = OutPoint::new(vec![], 0);

        insert_utxo(&mut utxo_set, outpoint.clone(), tx_out_1, 1);

        // Should panic, as we are trying to insert a UTXO with the same outpoint.
        insert_utxo(&mut utxo_set, outpoint, tx_out_2, 2);
    }

    #[test]
    fn malformed_addresses_are_not_inserted_in_address_outpoints() {
        // A script that isn't valid, but can be successfully converted into an address
        // due to a bug in the bitcoin crate. See:
        // (https://github.com/rust-bitcoin/rust-bitcoin/issues/995)
        let script = bitcoin::Script::from(vec![
            0, 17, 97, 69, 142, 51, 3, 137, 205, 4, 55, 238, 159, 227, 100, 29, 112, 204, 24,
        ]);

        let address = bitcoin::Address::from_script(&script, BitcoinNetwork::Testnet).unwrap();

        let mut utxo_set = UtxoSet::new(Network::Testnet);

        let tx_out_1 = TransactionBuilder::coinbase()
            .with_output(&address, 1000)
            .build()
            .output[0]
            .clone();

        insert_utxo(&mut utxo_set, OutPoint::new(vec![0; 32], 0), tx_out_1, 1);

        // Verify that this invalid address was not inserted into the address outpoints.
        assert!(utxo_set.address_to_outpoints.is_empty());
    }
}