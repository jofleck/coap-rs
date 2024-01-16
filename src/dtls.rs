//! this file is included by enabling the "dtls" feature. It provides a default DTLS backend using
//! webrtc-rs's dtls implementation.
use crate::client::Transport;
use crate::server::{Listener, Responder, TransportRequestSender};
use async_trait::async_trait;
use std::net::SocketAddr;
use std::time::Duration;
use std::{
    io::{Error, ErrorKind, Result},
    sync::Arc,
};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use webrtc_dtls::conn::DTLSConn;
use webrtc_util::conn::{Conn, Listener as WebRtcListener};

#[async_trait]
impl<L: WebRtcListener + Send + 'static> Listener for L {
    async fn listen(
        self: Box<Self>,
        sender: TransportRequestSender,
    ) -> std::io::Result<JoinHandle<std::io::Result<()>>> {
        Ok(tokio::spawn(async move {
            loop {
                let res = self.accept().await;
                if let Ok((dtls_conn, remote_addr)) = res {
                    tokio::spawn(spawn_webrtc_conn(dtls_conn, remote_addr, sender.clone()));
                } else {
                    return Err(std::io::Error::new(ErrorKind::Other, "could not accept"));
                }
            }
        }))
    }
}

pub struct DtlsResponse {
    pub conn: Arc<dyn Conn + Send + Sync>,
    pub remote_addr: SocketAddr,
}

#[async_trait]
impl Transport for DtlsConnection {
    async fn recv(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        let read = self
            .conn
            .read(buf, None)
            .await
            .map_err(|e| Error::new(ErrorKind::Other, e))?;
        let from = self.peer_addr;
        return Ok((read, from));
    }

    async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.conn
            .write(buf, None)
            .await
            .map_err(|e| Error::new(ErrorKind::Other, e))
    }
}
#[async_trait]
impl Responder for DtlsResponse {
    /// responds to a request by creating a new task. This ensures we do not
    /// block the main server handler task
    async fn respond(&self, response: Vec<u8>) {
        let self_clone = self.conn.clone();
        tokio::spawn(async move { self_clone.send(&response).await });
    }
    fn address(&self) -> SocketAddr {
        self.remote_addr
    }
}

pub async fn spawn_webrtc_conn(
    conn: Arc<dyn Conn + Send + Sync>,
    remote_addr: SocketAddr,
    sender: TransportRequestSender,
) {
    const VECTOR_LENGTH: usize = 1600;
    loop {
        let mut vec_buf = Vec::with_capacity(VECTOR_LENGTH);
        unsafe { vec_buf.set_len(VECTOR_LENGTH) };
        let Ok(rx) = conn.recv(&mut vec_buf).await else {
            break;
        };
        if rx == 0 || rx > VECTOR_LENGTH {
            break;
        }
        unsafe { vec_buf.set_len(rx) }
        let response = Arc::new(DtlsResponse {
            conn: conn.clone(),
            remote_addr,
        });
        let Ok(_) = sender.send((vec_buf, response)) else {
            break;
        };
    }
}

pub struct DtlsConnection {
    pub conn: DTLSConn,
    pub peer_addr: SocketAddr,
}

impl DtlsConnection {
    pub async fn try_new(dtls_config: DtlsConfig) -> Result<DtlsConnection> {
        let conn = Arc::new(
            UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|e| Error::new(ErrorKind::Other, e))?,
        );
        conn.connect(dtls_config.dest_addr)
            .await
            .map_err(|_| Error::new(ErrorKind::AddrNotAvailable, "address is in use"))?;
        let dtls_conn = timeout(
            Duration::new(30, 0),
            DTLSConn::new(conn, dtls_config.config, true, None),
        )
        .await
        .map_err(|_| {
            Error::new(
                ErrorKind::TimedOut,
                "Received no response on DTLS handshake",
            )
        })?
        .map_err(|e| Error::new(ErrorKind::Other, e))?;
        return Ok(DtlsConnection {
            conn: dtls_conn,
            peer_addr: dtls_config.dest_addr,
        });
    }
}
pub struct DtlsConfig {
    pub config: webrtc_dtls::config::Config,
    pub dest_addr: SocketAddr,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::client::CoAPClient;
    use crate::server::UdpCoapListener;
    use crate::{Server, UdpCoAPClient};
    use coap_lite::{CoapOption, CoapRequest, RequestType as Method};
    use futures::Future;
    use pkcs8::{LineEnding, SecretDocument};
    use rcgen::KeyPair;
    use std::fs::File;
    use std::io::{BufReader, Read};
    use std::net::{SocketAddr, ToSocketAddrs};
    use tokio::sync::mpsc;
    use webrtc_dtls::cipher_suite::CipherSuiteId;
    use webrtc_dtls::config::{ClientAuthType, Config, ExtendedMasterSecretType};
    use webrtc_dtls::crypto::{Certificate, CryptoPrivateKey};
    use webrtc_dtls::listener::listen;

    const SERVER_CERTIFICATE_PRIVATE_KEY: &'static str = "tests/test_certs/coap_server.pem";
    const SERVER_CERTIFICATE: &'static str = "tests/test_certs/coap_server.pub.pem";
    const CLIENT_CERTIFICATE_PRIVATE_KEY: &'static str = "tests/test_certs/coap_client.pem";
    const CLIENT_CERTIFICATE: &'static str = "tests/test_certs/coap_client.pub.pem";

    async fn request_handler(
        mut req: Box<CoapRequest<SocketAddr>>,
    ) -> Box<CoapRequest<SocketAddr>> {
        let uri_path_list = req.message.get_option(CoapOption::UriPath).unwrap().clone();
        assert_eq!(uri_path_list.len(), 1);

        match req.response {
            Some(ref mut response) => {
                response.message.payload = uri_path_list.front().unwrap().clone();
            }
            _ => {}
        }
        return req;
    }
    pub fn spawn_dtls_server<
        F: Fn(Box<CoapRequest<SocketAddr>>) -> HandlerRet + Send + Sync + 'static,
        HandlerRet,
    >(
        ip: &'static str,
        request_handler: F,
        config: webrtc_dtls::config::Config,
    ) -> mpsc::UnboundedReceiver<u16>
    where
        HandlerRet: Future<Output = Box<CoapRequest<SocketAddr>>> + Send,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let listener = listen(ip, config).await.unwrap();
            let listen_port = listener.addr().await.unwrap().port();
            let listener = Box::new(listener);
            let server = Server::from_listeners(vec![listener]);
            tx.send(listen_port).unwrap();
            server.run(request_handler).await.unwrap();
        });

        rx
    }

    pub fn get_certificate(name: &str) -> rustls::Certificate {
        let mut f = File::open(name).unwrap();
        let mut reader = BufReader::new(&mut f);
        let mut cert_iter = rustls_pemfile::certs(&mut reader);
        let cert = cert_iter
            .next()
            .unwrap()
            .expect("could not get certificate");
        assert!(
            cert_iter.next().is_none(),
            "there should only be 1 certificate in this file"
        );
        return rustls::Certificate(cert.to_vec());
    }

    pub fn server_certificate() -> rustls::Certificate {
        return get_certificate(SERVER_CERTIFICATE);
    }

    pub fn client_certificate() -> rustls::Certificate {
        return get_certificate(CLIENT_CERTIFICATE);
    }
    pub fn convert_to_pkcs8(s: &str) -> String {
        let pkdoc: SecretDocument =
            sec1::DecodeEcPrivateKey::from_sec1_pem(s).expect("could not parse ec key");

        let pkcs8_pem = pkdoc
            .to_pem("PRIVATE_KEY", LineEnding::LF)
            .expect("could not encode ec key to PEM");
        return pkcs8_pem.to_string();
    }

    pub fn get_private_key(name: &str) -> CryptoPrivateKey {
        let f = File::open(name).unwrap();
        let mut reader = BufReader::new(f);
        let mut buf = vec![];
        reader.read_to_end(&mut buf).unwrap();
        let s = String::from_utf8(buf).expect("utf8 of file");
        // convert key to pkcs8
        let s = convert_to_pkcs8(&s);

        let key_pair = KeyPair::from_pem(s.as_str()).expect("key pair in file");
        CryptoPrivateKey::from_key_pair(&key_pair).expect("crypto key pair")
    }

    pub fn server_key() -> CryptoPrivateKey {
        return get_private_key(SERVER_CERTIFICATE_PRIVATE_KEY);
    }

    pub fn client_key() -> CryptoPrivateKey {
        return get_private_key(CLIENT_CERTIFICATE_PRIVATE_KEY);
    }

    pub fn get_psk_config() -> Config {
        Config {
            psk: Some(Arc::new(|_| Ok(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 0]))),
            psk_identity_hint: Some("webrtc-rs DTLS Server".as_bytes().to_vec()),
            cipher_suites: vec![CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8],
            server_name: "localhost".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_dtls_pki() {
        let server_cfg = {
            let mut server_cert_pool = rustls::RootCertStore::empty();
            let server_cert = server_certificate();
            server_cert_pool
                .add(&server_cert)
                .expect("could not add certificate");

            let server_private_key = server_key();
            let certificate = Certificate {
                certificate: vec![server_cert],
                private_key: server_private_key,
            };

            Config {
                certificates: vec![certificate],
                extended_master_secret: ExtendedMasterSecretType::Require,
                client_auth: ClientAuthType::RequireAndVerifyClientCert, //RequireAnyClientCert, //
                client_cas: server_cert_pool,
                ..Default::default()
            }
        };

        let client_cfg = {
            let mut client_cert_pool = rustls::RootCertStore::empty();
            let client_cert = client_certificate();
            let server_cert = server_certificate();
            client_cert_pool
                .add(&server_cert)
                .expect("could not add certificate");

            let client_private_key = client_key();
            let certificate = Certificate {
                certificate: vec![client_cert],
                private_key: client_private_key,
            };

            Config {
                certificates: vec![certificate],
                extended_master_secret: ExtendedMasterSecretType::Require,
                roots_cas: client_cert_pool,
                server_name: "coap.rs".to_owned(),
                ..Default::default()
            }
        };

        let server_port = spawn_dtls_server("127.0.0.1:0", request_handler, server_cfg.clone())
            .recv()
            .await
            .unwrap();

        let dtls_config = DtlsConfig {
            config: client_cfg,
            dest_addr: ("127.0.0.1", server_port)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap(),
        };

        let mut client = CoAPClient::from_dtls_config(dtls_config)
            .await
            .expect("could not create client");
        let domain = format!("127.0.0.1:{}", server_port);
        let resp = client
            .request_path("/hello", Method::Get, None, None, Some(domain.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.message.payload, b"hello".to_vec());
    }
    #[tokio::test]
    async fn test_dtls_psk() {
        let config = get_psk_config();
        let server_port = spawn_dtls_server("127.0.0.1:0", request_handler, config.clone())
            .recv()
            .await
            .unwrap();

        let dtls_config = DtlsConfig {
            config,
            dest_addr: ("127.0.0.1", server_port)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap(),
        };

        let mut client = CoAPClient::from_dtls_config(dtls_config)
            .await
            .expect("could not create client");
        let domain = format!("127.0.0.1:{}", server_port);
        let resp = client
            .request_path("/hello", Method::Get, None, None, Some(domain.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.message.payload, b"hello".to_vec());
    }
    #[tokio::test]
    async fn test_dtls_false_psk() {
        let mut config = get_psk_config();
        let server_port = spawn_dtls_server("127.0.0.1:0", request_handler, config.clone())
            .recv()
            .await
            .unwrap();
        // make the psk fail
        config.psk = Some(Arc::new(|_| Ok(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 9])));

        let dtls_config = DtlsConfig {
            config,
            dest_addr: ("127.0.0.1", server_port)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap(),
        };
        assert!(
            CoAPClient::from_dtls_config(dtls_config).await.is_err(),
            "should not have connected"
        );
    }

    #[tokio::test]
    async fn test_wrong_protocol() {
        let config = get_psk_config();
        let server_port = spawn_dtls_server("127.0.0.1:0", request_handler, config.clone())
            .recv()
            .await
            .unwrap();
        // make the psk fail

        let get = UdpCoAPClient::get(&format!("coap://127.0.0.1:{}/hello", server_port)).await;
        let get_error = get.unwrap_err();
        assert!(get_error.kind() == ErrorKind::TimedOut);

        let dtls_config = DtlsConfig {
            config,
            dest_addr: ("127.0.0.1", server_port)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap(),
        };

        let mut client = CoAPClient::from_dtls_config(dtls_config)
            .await
            .expect("could not create client");
        let domain = format!("127.0.0.1:{}", server_port);
        let resp = client
            .request_path("/hello", Method::Get, None, None, Some(domain.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.message.payload, b"hello".to_vec());
    }

    #[tokio::test]
    async fn test_multiple_listeners() {
        let config = get_psk_config();
        // spawn a server with 2 listeners
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let config = get_psk_config();
            let listener_dtls = listen("127.0.0.1:0", config).await.unwrap();
            let port_dtls = listener_dtls.addr().await.unwrap().port();
            let listener_dtls = Box::new(listener_dtls);

            let sock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port_udp = sock_udp.local_addr().unwrap().port();
            let listener_udp = Box::new(UdpCoapListener::from_socket(sock_udp));

            let server = Server::from_listeners(vec![listener_dtls, listener_udp]);
            tx.send((port_udp, port_dtls)).unwrap();
            server.run(request_handler).await.unwrap();
        });

        let (udp, dtls) = rx.recv().await.unwrap();

        let get = UdpCoAPClient::get(&format!("coap://127.0.0.1:{}/hello_udp", udp))
            .await
            .unwrap();
        assert_eq!(get.message.payload, b"hello_udp".to_vec());

        let dtls_config = DtlsConfig {
            config,
            dest_addr: ("127.0.0.1", dtls)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap(),
        };

        let mut client = CoAPClient::from_dtls_config(dtls_config)
            .await
            .expect("could not create client");
        let domain = format!("127.0.0.1:{}", dtls);
        let resp = client
            .request_path(
                "/hello_dtls",
                Method::Get,
                None,
                None,
                Some(domain.to_string()),
            )
            .await
            .unwrap();
        assert_eq!(resp.message.payload, b"hello_dtls".to_vec());
    }
}