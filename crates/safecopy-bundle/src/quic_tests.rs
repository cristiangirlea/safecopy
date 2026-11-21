//! Bundles over a local QUIC pair (the `quic` feature).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use quinn::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use safecopy_folder::Directory;

use super::tests::{data, entries, files_in, save, temp};
use super::*;

/// A server and a client endpoint on the loopback, and a connection each
/// way between them.
async fn pair() -> (quinn::Endpoint, quinn::Connection, quinn::Connection) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = CertificateDer::from(certified.cert);
    let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let server_config =
        quinn::ServerConfig::with_single_cert(vec![cert.clone()], key.into()).unwrap();
    let server = quinn::Endpoint::server(server_config, loopback).unwrap();
    let mut roots = quinn::rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let mut client = quinn::Endpoint::client(loopback).unwrap();
    client.set_default_client_config(
        quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap(),
    );
    let address = server.local_addr().unwrap();
    let (accepted, connected) = tokio::join!(
        async { server.accept().await.unwrap().await.unwrap() },
        async { client.connect(address, "localhost").unwrap().await.unwrap() }
    );
    (server, accepted, connected)
}

#[tokio::test]
async fn a_bundle_is_saved_over_a_local_quic_pair() {
    let path = temp("quic");
    let dir = Directory::open(&path).unwrap();
    let files = [data(100, 1), data(300_000, 2), data(1 << 20, 3), data(0, 4)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let (_server, receiver, sender) = pair().await;
    let sending = async {
        let send = sender.open_uni().await.unwrap();
        let mut writer = BundleWriter::new(
            send,
            &entries(&refs),
            &BundleLimits::default(),
            WriteOptions::default(),
        )
        .await
        .unwrap();
        writer.write(&mut &files[0][..]).await.unwrap();
        writer.give_up().await.unwrap();
        writer.write(&mut &files[2][..]).await.unwrap();
        writer.write(&mut &files[3][..]).await.unwrap();
        let send = writer.finish().await.unwrap();
        // The receiver read it all: no code.
        quic::stopped(&send).await
    };
    let receiving = async {
        let recv = receiver.accept_uni().await.unwrap();
        let table = BundleReader::open(recv, &BundleLimits::default())
            .await
            .unwrap();
        let decisions = ["a", "b", "c", "d"].iter().map(|n| save(&dir, n)).collect();
        let options = ReadOptions {
            sync: false,
            ..ReadOptions::default()
        };
        table
            .read(decisions, &options, &(), std::future::pending())
            .await
    };
    let (code, report) = tokio::join!(sending, receiving);
    assert_eq!(code, None);
    assert_eq!(report.error, None);
    assert_eq!(report.outcomes[1], Outcome::GivenUp);
    assert_eq!(files_in(&path), ["a", "c", "d"]);
    assert_eq!(std::fs::read(path.join("c")).unwrap(), files[2]);
}

#[tokio::test]
async fn a_refused_bundle_stops_the_quic_stream_with_the_programs_code() {
    const REFUSED: u32 = 3;
    let (_server, receiver, sender) = pair().await;
    let sending = async {
        let mut send = sender.open_uni().await.unwrap();
        // A table naming 65 entries.
        let mut table = 65u32.to_le_bytes().to_vec();
        for _ in 0..65 {
            table.extend_from_slice(&[0; 8 + 16 + 4]);
        }
        send.write_all(&[safecopy_chunks::FRAMING]).await.unwrap();
        send.write_all(&(table.len() as u32).to_le_bytes())
            .await
            .unwrap();
        send.write_all(&table).await.unwrap();
        quic::stopped(&send).await
    };
    let receiving = async {
        let mut recv = receiver.accept_uni().await.unwrap();
        let refused = BundleReader::open(&mut recv, &BundleLimits::default()).await;
        assert!(matches!(
            refused,
            Err(BundleError::TooMany { count: 65, .. })
        ));
        assert!(quic::stop(&mut recv, REFUSED));
    };
    let (code, ()) = tokio::join!(sending, receiving);
    assert_eq!(code, Some(REFUSED));
}
