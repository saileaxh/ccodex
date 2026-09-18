use anyhow::{Context as _, bail};
use shadowsocks::config::ServerType;
use shadowsocks::context::Context;
use shadowsocks::relay::socks5::{
    Command, HandshakeRequest, HandshakeResponse, PasswdAuthRequest, PasswdAuthResponse, Reply,
    TcpRequestHeader, TcpResponseHeader,
};
use shadowsocks::{ProxyClientStream, ServerConfig};
use std::future::Future;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONNECTIONS: usize = 1024;

pub fn is_shadowsocks(url: &str) -> bool {
    url.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ss://"))
}

pub fn parse_server(url: &str) -> anyhow::Result<ServerConfig> {
    let server = ServerConfig::from_url(url).map_err(|_| {
        anyhow::anyhow!("Shadowsocks 地址无效，请检查 ss:// 地址、加密方法和密钥长度")
    })?;
    if server.plugin().is_some() {
        bail!("内置 Shadowsocks 不支持 SIP003 插件");
    }
    if !server.method().is_aead() && !server.method().is_aead_2022() {
        bail!("内置 Shadowsocks 仅支持 AEAD / AEAD-2022 加密方法");
    }
    Ok(server)
}

pub struct EmbeddedProxy {
    listener: StdTcpListener,
    server: ServerConfig,
    password: String,
    url: String,
}

impl EmbeddedProxy {
    pub fn prepare(upstream: Option<&str>) -> anyhow::Result<Option<Self>> {
        let Some(upstream) = upstream.filter(|url| is_shadowsocks(url)) else {
            return Ok(None);
        };
        let server = parse_server(upstream)?;
        let listener =
            StdTcpListener::bind("127.0.0.1:0").context("无法创建内置 Shadowsocks 转发端口")?;
        listener.set_nonblocking(true)?;
        let password = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let url = format!("socks5h://ccodex:{password}@{}", listener.local_addr()?);
        Ok(Some(Self {
            listener,
            server,
            password,
            url,
        }))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let listener = TcpListener::from_std(self.listener)?;
        let server = Arc::new(self.server);
        let password = Arc::new(self.password);
        let context = Context::new_shared(ServerType::Local);
        let mut connections = JoinSet::new();
        tracing::info!(method = %server.method(), "embedded Shadowsocks proxy ready");
        loop {
            tokio::select! {
                accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                    let (stream, _) = accepted.context("内置 Shadowsocks 监听失败")?;
                    let server = Arc::clone(&server);
                    let password = Arc::clone(&password);
                    let context = Arc::clone(&context);
                    connections.spawn(async move {
                        relay(stream, &server, &password, context).await
                    });
                }
                finished = connections.join_next(), if !connections.is_empty() => {
                    match finished {
                        Some(Ok(Err(error))) => tracing::debug!(%error, "Shadowsocks connection closed"),
                        Some(Err(error)) => tracing::warn!(%error, "Shadowsocks connection task failed"),
                        _ => {}
                    }
                }
            }
        }
    }
}

pub async fn with_proxy<T>(
    proxy: Option<EmbeddedProxy>,
    operation: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match proxy {
        Some(proxy) => tokio::select! {
            result = operation => result,
            result = proxy.run() => {
                result?;
                bail!("内置 Shadowsocks 意外停止")
            }
        },
        None => operation.await,
    }
}

async fn handshake(stream: &mut TcpStream, password: &str) -> anyhow::Result<TcpRequestHeader> {
    let request = HandshakeRequest::read_from(stream).await?;
    if !request.methods.contains(&2) {
        HandshakeResponse::new(255).write_to(stream).await?;
        bail!("SOCKS authentication required");
    }
    HandshakeResponse::new(2).write_to(stream).await?;
    let auth = PasswdAuthRequest::read_from(stream).await?;
    let authenticated = crate::util::ct_eq(&auth.uname, b"ccodex")
        & crate::util::ct_eq(&auth.passwd, password.as_bytes());
    PasswdAuthResponse::new(if authenticated { 0 } else { 1 })
        .write_to(stream)
        .await?;
    if !authenticated {
        bail!("SOCKS authentication failed");
    }
    let request = TcpRequestHeader::read_from(stream).await?;
    if !matches!(request.command, Command::TcpConnect) {
        reply(stream, Reply::CommandNotSupported).await?;
        bail!("only TCP CONNECT is supported");
    }
    Ok(request)
}

async fn reply(stream: &mut TcpStream, status: Reply) -> anyhow::Result<()> {
    TcpResponseHeader::new(status, "0.0.0.0:0".parse().unwrap())
        .write_to(stream)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shadowsocks::crypto::CipherKind;
    use shadowsocks::relay::socks5::Address;
    use shadowsocks::relay::tcprelay::proxy_stream::server::ProxyServerStream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn validates_cipher_key_and_rejects_plugins_without_exposing_secrets() {
        assert!(parse_server("ss://aes-256-gcm:test-password@127.0.0.1:8388").is_ok());
        assert!(
            parse_server(
                "ss://2022-blake3-aes-128-gcm:AAAAAAAAAAAAAAAAAAAAAA%3D%3D@127.0.0.1:8388"
            )
            .is_ok()
        );
        for url in [
            "ss://2022-blake3-aes-128-gcm:secret@127.0.0.1:8388",
            "ss://unknown:secret@127.0.0.1:8388",
            "ss://none:secret@127.0.0.1:8388",
            "ss://aes-256-gcm:secret@127.0.0.1:8388/?plugin=v2ray-plugin",
        ] {
            let error = parse_server(url).unwrap_err().to_string();
            assert!(!error.contains("secret"));
            assert_eq!(crate::util::redact_url_credentials(url), "ss://***");
        }
        assert!(
            EmbeddedProxy::prepare(Some("http://localhost:8080"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::util::redact_url_credentials("ss://YWVzLTI1Ni1nY206c2VjcmV0QGhvc3Q6ODM4OA"),
            "ss://***"
        );
    }

    async fn authenticate(stream: &mut TcpStream, password: &str) {
        HandshakeRequest::new(vec![2])
            .write_to(stream)
            .await
            .unwrap();
        assert_eq!(
            HandshakeResponse::read_from(stream)
                .await
                .unwrap()
                .chosen_method,
            2
        );
        PasswdAuthRequest::new(b"ccodex".to_vec(), password.as_bytes().to_vec())
            .write_to(stream)
            .await
            .unwrap();
        assert_eq!(
            PasswdAuthResponse::read_from(stream).await.unwrap().status,
            0
        );
    }

    #[tokio::test]
    async fn rejects_unauthenticated_clients_and_udp() {
        let proxy = EmbeddedProxy::prepare(Some("ss://aes-256-gcm:test@127.0.0.1:1"))
            .unwrap()
            .unwrap();
        let address = proxy.listener.local_addr().unwrap();
        assert!(address.ip().is_loopback());
        let password = proxy.password.clone();
        with_proxy(Some(proxy), async {
            let mut anonymous = TcpStream::connect(address).await?;
            HandshakeRequest::new(vec![0])
                .write_to(&mut anonymous)
                .await?;
            assert_eq!(
                HandshakeResponse::read_from(&mut anonymous)
                    .await?
                    .chosen_method,
                255
            );
            let mut invalid = TcpStream::connect(address).await?;
            HandshakeRequest::new(vec![2])
                .write_to(&mut invalid)
                .await?;
            HandshakeResponse::read_from(&mut invalid).await?;
            PasswdAuthRequest::new(b"ccodex".to_vec(), b"wrong".to_vec())
                .write_to(&mut invalid)
                .await?;
            assert_eq!(PasswdAuthResponse::read_from(&mut invalid).await?.status, 1);
            let mut udp = TcpStream::connect(address).await?;
            authenticate(&mut udp, &password).await;
            TcpRequestHeader::new(Command::UdpAssociate, "127.0.0.1:53".parse()?)
                .write_to(&mut udp)
                .await?;
            assert!(matches!(
                TcpResponseHeader::read_from(&mut udp).await?.reply,
                Reply::CommandNotSupported
            ));
            Ok(())
        })
        .await
        .unwrap();
        assert!(TcpStream::connect(address).await.is_err());
    }

    async fn encrypted_roundtrip(method: CipherKind, password: &str) {
        let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server =
            ServerConfig::new(server_listener.local_addr().unwrap(), password, method).unwrap();
        let proxy = EmbeddedProxy::prepare(Some(&server.to_url()))
            .unwrap()
            .unwrap();
        let proxy_url = proxy.url().to_string();
        let address = proxy.listener.local_addr().unwrap();
        let local_password = proxy.password.clone();
        let payload: Vec<u8> = (0..131_072).map(|index| (index % 251) as u8).collect();
        let server_payload = payload.clone();
        let server_task = async move {
            let context = Context::new_shared(ServerType::Server);
            let (stream, _) = server_listener.accept().await.unwrap();
            let mut encrypted =
                ProxyServerStream::from_stream(context, stream, method, server.key());
            assert_eq!(
                encrypted.handshake().await.unwrap(),
                Address::DomainNameAddress("resolve-remotely.invalid".into(), 443)
            );
            let mut received = Vec::new();
            encrypted.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, server_payload);
            encrypted.write_all(&received).await.unwrap();
            encrypted.shutdown().await.unwrap();

            let (stream, _) = server_listener.accept().await.unwrap();
            let mut encrypted = ProxyServerStream::from_stream(
                Context::new_shared(ServerType::Server),
                stream,
                method,
                server.key(),
            );
            assert_eq!(
                encrypted.handshake().await.unwrap(),
                Address::DomainNameAddress("resolve-remotely.invalid".into(), 80)
            );
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(encrypted.read_u8().await.unwrap());
            }
            assert!(request.starts_with(b"GET /events HTTP/1.1\r\n"));
            let events = b"data: first\n\ndata: [DONE]\n\n";
            encrypted.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", events.len()).as_bytes()).await.unwrap();
            for chunk in events.chunks(3) {
                encrypted.write_all(chunk).await.unwrap();
                encrypted.flush().await.unwrap();
            }
            encrypted.shutdown().await.unwrap();
        };
        let client_task = with_proxy(Some(proxy), async {
            let mut stream = TcpStream::connect(address).await?;
            authenticate(&mut stream, &local_password).await;
            TcpRequestHeader::new(
                Command::TcpConnect,
                Address::DomainNameAddress("resolve-remotely.invalid".into(), 443),
            )
            .write_to(&mut stream)
            .await?;
            assert!(matches!(
                TcpResponseHeader::read_from(&mut stream).await?.reply,
                Reply::Succeeded
            ));
            stream.write_all(&payload).await?;
            stream.shutdown().await?;
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await?;
            assert_eq!(received, payload);
            let response = reqwest::Client::builder()
                .no_proxy()
                .proxy(reqwest::Proxy::all(proxy_url)?)
                .build()?
                .get("http://resolve-remotely.invalid/events")
                .send()
                .await?;
            assert_eq!(response.status(), 200);
            assert_eq!(response.text().await?, "data: first\n\ndata: [DONE]\n\n");
            Ok(())
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            let (_, result) = tokio::join!(server_task, client_task);
            result.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn aead_2022_roundtrip_preserves_streams_and_remote_dns() {
        encrypted_roundtrip(
            CipherKind::AEAD2022_BLAKE3_AES_128_GCM,
            "AAAAAAAAAAAAAAAAAAAAAA==",
        )
        .await;
    }

    #[tokio::test]
    async fn aead_roundtrip_preserves_streams_and_remote_dns() {
        encrypted_roundtrip(CipherKind::AES_256_GCM, "test-password").await;
    }
}

async fn relay(
    mut stream: TcpStream,
    server: &ServerConfig,
    password: &str,
    context: shadowsocks::context::SharedContext,
) -> anyhow::Result<()> {
    let request = tokio::time::timeout(CONNECT_TIMEOUT, handshake(&mut stream, password))
        .await
        .context("SOCKS handshake timed out")??;
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        ProxyClientStream::connect(context, server, request.address),
    )
    .await;
    let mut upstream = match connected {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(error)) => {
            reply(&mut stream, Reply::HostUnreachable).await?;
            return Err(error.into());
        }
        Err(error) => {
            reply(&mut stream, Reply::TtlExpired).await?;
            return Err(error.into());
        }
    };
    reply(&mut stream, Reply::Succeeded).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
    Ok(())
}
